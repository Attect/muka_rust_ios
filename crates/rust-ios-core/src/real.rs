use std::collections::BTreeMap;
use std::io;
use std::io::Cursor;
#[cfg(windows)]
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::TimeZone;
use glob::Pattern;
use idevice::core_device_proxy::CoreDeviceProxy;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use idevice::tcp::handle::AdapterHandle;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdDevice};
use idevice::{IdeviceService, RsdService};
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult,
    ConnectionPreference, ConnectionType, DebugAppInfo, DeviceInfo, DeviceList, DeviceStateSpec,
    LocationSimulationSpec, OsTraceLevel, OsTraceOptions, RemotePairingInfo, StreamFormat,
    StreamTaskSpec, TaskKind, TunnelPairSpec, TunnelSpec, TunnelTransport, UiRunnerSpec, WdaSpec,
    WebInspectorCdpSpec, WebInspectorJsShellSpec, XCTestEnvironmentValue, XCTestEvent,
    XCTestEventKind, XCTestRunSummary, XCTestSpec, XCTestTargetSpec,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::activation::{
    DEVICE_ACTIVATION_URL, DRM_HANDSHAKE_URL, MobileActivationClient, activation_info_request,
    ensure_response_success, handle_activation_request, http_client, response_value_dictionary,
    send_activation_info, send_drm_handshake, session_info_request, validate_session_info,
};
use crate::ddi::{download_legacy_ddi, find_best_personalized_ddi, load_personalized_ddi};
use crate::management::{
    PairInvocation, SETUP_SKIP_KEYS, SetWallpaperInvocation, WallpaperScreen,
    parse_pair_invocation, parse_set_wallpaper_invocation, validate_erase_invocation,
};
use crate::mcinstall::{
    HTTP_PROXY_PROFILE_IDENTIFIER, McInstallClient, http_proxy_profile, initial_prepare_profile,
    wifi_profile, wifi_profile_identifier,
};
use crate::remote_pairing::{pair_remote_tunnel, start_remote_tunnel};
use crate::springboard::{parse_app_icon_request, write_png_atomic};
use crate::supervision::{certificate_from_identity, generate_identity, sign_pkcs12_challenge};
use crate::{
    AppProcessSession, AppProcessSessionInfo, BoxedIo, CoreError, CoreResult, DeviceBackend,
    DeviceCondition, DeviceConditionInfo, DeviceLocationSimulation, DeviceStream, DeviceStreamInfo,
    DeviceTarget, KernelTunnelHandle, KernelTunnelInfo, KernelTunnelRunner, LocationSimulationInfo,
    RsdServiceInfo, RsdServiceKind, ServiceTransport, UiRunnerSession, UiRunnerSessionInfo,
    UserSpaceTunnel, UserSpaceTunnelInfo, WdaSession, WdaSessionInfo, WebInspectorCdpSession,
    WebInspectorJsShellSession, XCTestSession, XCTestSessionInfo, XCTestSessionUpdate,
};

/// 基于纯 Rust `idevice` 的真实 usbmuxd/Lockdown/CoreDevice backend。
#[derive(Debug)]
pub struct IdeviceBackend {
    addr: UsbmuxdAddr,
    next_tag: AtomicU32,
}

impl Default for IdeviceBackend {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl IdeviceBackend {
    pub fn from_environment() -> Self {
        Self {
            addr: UsbmuxdAddr::from_env_var().unwrap_or_default(),
            next_tag: AtomicU32::new(1),
        }
    }

    pub fn new(addr: UsbmuxdAddr) -> Self {
        Self {
            addr,
            next_tag: AtomicU32::new(1),
        }
    }

    fn tag(&self) -> u32 {
        self.next_tag.fetch_add(1, Ordering::Relaxed)
    }

    async fn raw_devices(&self) -> CoreResult<Vec<UsbmuxdDevice>> {
        let mut connection = self
            .addr
            .connect(self.tag())
            .await
            .map_err(map_usbmuxd_error)?;
        connection.get_devices().await.map_err(map_usbmuxd_error)
    }

    async fn raw_device(&self, target: &DeviceTarget) -> CoreResult<UsbmuxdDevice> {
        let mut devices: Vec<UsbmuxdDevice> = self
            .raw_devices()
            .await?
            .into_iter()
            .filter(|device| target.udid.as_ref().is_none_or(|udid| device.udid == *udid))
            .filter(|device| match target.connection {
                ConnectionPreference::UsbOnly => device.connection_type == Connection::Usb,
                ConnectionPreference::NetworkOnly => {
                    matches!(device.connection_type, Connection::Network(_))
                }
                ConnectionPreference::UsbPreferred | ConnectionPreference::Any => true,
            })
            .collect();

        if devices.is_empty() {
            return Err(CoreError::DeviceNotFound(
                target
                    .udid
                    .clone()
                    .unwrap_or_else(|| "no connected iOS device".into()),
            ));
        }

        if target.connection == ConnectionPreference::UsbPreferred
            && let Some(index) = devices
                .iter()
                .position(|device| device.connection_type == Connection::Usb)
        {
            return Ok(devices.swap_remove(index));
        }
        Ok(devices.swap_remove(0))
    }
}

#[async_trait]
impl DeviceBackend for IdeviceBackend {
    fn name(&self) -> &'static str {
        "idevice"
    }

    async fn list_devices(&self) -> CoreResult<DeviceList> {
        Ok(DeviceList {
            devices: self
                .raw_devices()
                .await?
                .into_iter()
                .map(raw_device_info)
                .collect(),
        })
    }

    async fn read_buid(&self) -> CoreResult<String> {
        let mut connection = self
            .addr
            .connect(self.tag())
            .await
            .map_err(map_usbmuxd_error)?;
        connection.get_buid().await.map_err(map_usbmuxd_error)
    }

    async fn read_pair_record(&self, udid: &str) -> CoreResult<Value> {
        let mut connection = self
            .addr
            .connect(self.tag())
            .await
            .map_err(map_usbmuxd_error)?;
        let record = connection
            .get_pair_record(udid)
            .await
            .map_err(|error| map_pairing_error(udid, error))?;

        Ok(json!({
            "DeviceCertificate": BASE64.encode(record.device_certificate.as_ref()),
            "HostPrivateKey": BASE64.encode(&record.host_private_key),
            "HostCertificate": BASE64.encode(record.host_certificate.as_ref()),
            "RootPrivateKey": BASE64.encode(&record.root_private_key),
            "RootCertificate": BASE64.encode(record.root_certificate.as_ref()),
            "SystemBUID": record.system_buid,
            "HostID": record.host_id,
            "EscrowBag": record.escrow_bag.map(|value| BASE64.encode(value)),
            "WiFiMACAddress": record.wifi_mac_address,
            "UDID": record.udid,
            "_encoding": "base64"
        }))
    }

    async fn connect_port(&self, target: &DeviceTarget, port: u16) -> CoreResult<BoxedIo> {
        if port == 0 {
            return Err(CoreError::InvalidArgument(
                "device port must be between 1 and 65535".into(),
            ));
        }
        let device = self.raw_device(target).await?;
        match device.connection_type {
            Connection::Network(address) => {
                let stream = TcpStream::connect((address, port))
                    .await
                    .map_err(CoreError::Io)?;
                Ok(Box::new(stream))
            }
            Connection::Usb => {
                let connection = self
                    .addr
                    .connect(self.tag())
                    .await
                    .map_err(map_usbmuxd_error)?;
                let idevice = connection
                    .connect_to_device(device.device_id, port, "rust-ios-forward")
                    .await
                    .map_err(map_device_error)?;
                let socket = idevice.get_socket().ok_or_else(|| {
                    CoreError::DeviceUnavailable("device connection has no socket".into())
                })?;
                Ok(Box::new(socket))
            }
            Connection::Unknown(description) => Err(CoreError::DeviceUnavailable(format!(
                "unsupported usbmuxd connection type: {description}"
            ))),
        }
    }

    async fn start_userspace_tunnel(
        &self,
        spec: &TunnelSpec,
    ) -> CoreResult<Box<dyn UserSpaceTunnel>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let udid = device.udid.clone();
        if spec.transport == TunnelTransport::RemotePairingTcp {
            let parts = start_remote_tunnel(&udid, Path::new(&spec.pair_record_path)).await?;
            return Ok(Box::new(IdeviceUserSpaceTunnel {
                info: parts.info,
                adapter: Mutex::new(Some(parts.adapter)),
                rsd: Mutex::new(None),
            }));
        }
        let provider = device.to_provider(self.addr.clone(), "rust-ios-tunnel");
        let proxy = CoreDeviceProxy::connect(&provider)
            .await
            .map_err(map_device_error)?;
        let tunnel_info = proxy.tunnel_info().clone();
        let adapter = proxy
            .create_software_tunnel()
            .map_err(map_device_error)?
            .to_async_handle();

        Ok(Box::new(IdeviceUserSpaceTunnel {
            info: UserSpaceTunnelInfo {
                udid,
                client_address: tunnel_info.client_address,
                server_address: tunnel_info.server_address,
                netmask: tunnel_info.netmask,
                mtu: tunnel_info.mtu,
                rsd_port: tunnel_info.server_rsd_port,
            },
            adapter: Mutex::new(Some(adapter)),
            rsd: Mutex::new(None),
        }))
    }

    async fn start_kernel_tunnel(&self, spec: &TunnelSpec) -> CoreResult<KernelTunnelHandle> {
        #[cfg(windows)]
        {
            start_windows_kernel_tunnel(self, spec).await
        }
        #[cfg(not(windows))]
        {
            let _ = spec;
            Err(CoreError::UnsupportedPlatform(
                "kernel tunnel currently requires Windows and Wintun".into(),
            ))
        }
    }

    async fn pair_remote_tunnel(&self, spec: &TunnelPairSpec) -> CoreResult<RemotePairingInfo> {
        if !spec.manual {
            return Err(CoreError::InvalidArgument(
                "RemotePairing requires explicit manual authorization".into(),
            ));
        }
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        pair_remote_tunnel(&device.udid, Path::new(&spec.pair_record_path)).await
    }

    async fn start_stream(
        &self,
        spec: &StreamTaskSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let provider = device.to_provider(self.addr.clone(), "rust-ios-stream");
        match &spec.kind {
            TaskKind::Syslog => {
                if spec.format == StreamFormat::Pcap {
                    return Err(CoreError::InvalidArgument(
                        "syslog does not support pcap format".into(),
                    ));
                }
                let client = idevice::syslog_relay::SyslogRelayClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                Ok(Box::new(IdeviceSyslogStream {
                    client,
                    format: spec.format.clone(),
                    info: DeviceStreamInfo {
                        content_type: if spec.format == StreamFormat::JsonLines {
                            "application/x-ndjson"
                        } else {
                            "text/plain; charset=utf-8"
                        }
                        .into(),
                        initial_bytes: Vec::new(),
                        service_transport: ServiceTransport::Lockdown,
                    },
                }))
            }
            TaskKind::OsTrace => {
                if spec.format == StreamFormat::Pcap {
                    return Err(CoreError::InvalidArgument(
                        "ostrace does not support pcap format".into(),
                    ));
                }
                let pid = match (spec.pid, spec.process.as_deref()) {
                    (Some(pid), _) => Some(pid),
                    (None, Some(process)) => {
                        Some(resolve_process_pid(&provider, process, tunnel.as_deref()).await?)
                    }
                    (None, None) => None,
                };
                let client = idevice::os_trace_relay::OsTraceRelayClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                let receiver = client.start_trace(pid).await.map_err(map_device_error)?;
                Ok(Box::new(IdeviceOsTraceStream {
                    receiver,
                    format: spec.format.clone(),
                    options: spec.os_trace.clone().unwrap_or_default(),
                    info: DeviceStreamInfo {
                        content_type: if spec.format == StreamFormat::JsonLines {
                            "application/x-ndjson"
                        } else {
                            "text/plain; charset=utf-8"
                        }
                        .into(),
                        initial_bytes: Vec::new(),
                        service_transport: ServiceTransport::Lockdown,
                    },
                }))
            }
            TaskKind::Pcap => {
                if spec.format != StreamFormat::Pcap {
                    return Err(CoreError::InvalidArgument(
                        "pcap stream requires pcap format".into(),
                    ));
                }
                let client = idevice::pcapd::PcapdClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                Ok(Box::new(IdevicePcapStream {
                    client,
                    pid: spec.pid,
                    process: spec.process.clone(),
                    info: DeviceStreamInfo {
                        content_type: "application/vnd.tcpdump.pcap".into(),
                        initial_bytes: pcap_header(),
                        service_transport: ServiceTransport::Lockdown,
                    },
                }))
            }
            TaskKind::ScreenshotStream => {
                if spec.format != StreamFormat::Raw {
                    return Err(CoreError::InvalidArgument(
                        "screenshot stream requires raw frame format".into(),
                    ));
                }
                start_screenshot_dvt_stream(provider, tunnel).await
            }
            TaskKind::Notifications => {
                if spec.format != StreamFormat::JsonLines {
                    return Err(CoreError::InvalidArgument(
                        "instruments notifications requires JSON Lines format".into(),
                    ));
                }
                start_notifications_stream(provider, tunnel).await
            }
            TaskKind::Sysmontap => {
                if spec.format != StreamFormat::JsonLines {
                    return Err(CoreError::InvalidArgument(
                        "sysmontap requires JSON Lines format".into(),
                    ));
                }
                start_sysmontap_stream(provider, tunnel).await
            }
            kind => Err(CoreError::NotImplemented(format!(
                "stream kind {kind:?} has not been migrated"
            ))),
        }
    }

    async fn start_accessibility_inspector(
        &self,
        spec: &AccessibilityInspectorSpec,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let provider = device.to_provider(self.addr.clone(), "rust-ios-accessibility-inspector");
        crate::accessibility::start_inspector(&provider, spec).await
    }

    async fn start_device_condition(
        &self,
        spec: &DeviceStateSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceCondition>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let provider = device.to_provider(self.addr.clone(), "rust-ios-device-state");
        let service_transport = dvt_service_transport(tunnel.as_ref());
        let mut remote = connect_dvt(&provider, tunnel.as_deref()).await?;
        let description = {
            let mut control =
                idevice::dvt::condition_inducer::ConditionInducerClient::new(&mut remote)
                    .await
                    .map_err(map_device_error)?;
            let groups = control
                .available_conditions()
                .await
                .map_err(map_device_error)?;
            let profile = groups
                .iter()
                .find(|group| group.identifier == spec.condition_identifier)
                .and_then(|group| {
                    group
                        .profiles
                        .iter()
                        .find(|profile| profile.identifier == spec.profile_identifier)
                })
                .ok_or_else(|| {
                    CoreError::InvalidArgument(format!(
                        "device condition {:?} has no profile {:?}",
                        spec.condition_identifier, spec.profile_identifier
                    ))
                })?;
            let description = profile.description.clone();
            control
                .enable_condition(&spec.condition_identifier, &spec.profile_identifier)
                .await
                .map_err(map_device_error)?;
            description
        };
        Ok(Box::new(IdeviceCondition {
            info: DeviceConditionInfo {
                udid: device.udid,
                condition_identifier: spec.condition_identifier.clone(),
                profile_identifier: spec.profile_identifier.clone(),
                profile_description: description,
                service_transport,
            },
            remote: Mutex::new(Some(remote)),
        }))
    }

    async fn start_location_simulation(
        &self,
        spec: &LocationSimulationSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceLocationSimulation>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let provider = device.to_provider(self.addr.clone(), "rust-ios-location-simulation");
        let mut lockdown = lockdown_with_session(&provider).await?;
        let version = lockdown_string_value(&mut lockdown, "ProductVersion", None).await?;
        drop(lockdown);
        let use_dvt = tunnel.is_some() || ios_major_version(&version)? >= 17;
        let (transport, service_transport) = if use_dvt {
            (
                IdeviceLocationTransport::Dvt(connect_dvt(&provider, tunnel.as_deref()).await?),
                dvt_service_transport(tunnel.as_ref()),
            )
        } else {
            (
                IdeviceLocationTransport::Classic(provider),
                ServiceTransport::Lockdown,
            )
        };
        Ok(Box::new(IdeviceLocationSimulation {
            info: LocationSimulationInfo {
                udid: device.udid,
                service_transport,
            },
            transport: Some(transport),
        }))
    }

    async fn start_wda_session(
        &self,
        spec: &WdaSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WdaSession>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let udid = device.udid.clone();
        let (shared_rsd, service_transport) = shared_xctest_rsd(tunnel, &udid)?;
        let provider: Arc<dyn IdeviceProvider> =
            Arc::new(device.to_provider(self.addr.clone(), "rust-ios-wda"));
        let mut install = idevice::installation_proxy::InstallationProxyClient::connect(&*provider)
            .await
            .map_err(map_device_error)?;
        let target_bundle_id = spec
            .target_bundle_id
            .as_deref()
            .unwrap_or(&spec.runner_bundle_id);
        let mut config = idevice::dvt::xctest::TestConfig::from_installation_proxy(
            &mut install,
            &spec.runner_bundle_id,
            Some(target_bundle_id),
        )
        .await
        .map_err(map_device_error)?;
        let derived_xctest_config = format!("{}.xctest", config.config_name());
        if let Some(requested) = spec.xctest_config.as_deref()
            && requested != derived_xctest_config
        {
            return Err(CoreError::InvalidArgument(format!(
                "xctest config {requested:?} does not match installed runner config {derived_xctest_config:?}"
            )));
        }
        if !spec.arguments.is_empty() {
            config.runner_args = Some(spec.arguments.clone());
        }
        if !spec.environment.is_empty() {
            config.runner_env = Some(
                spec.environment
                    .iter()
                    .map(|(key, value)| (key.clone(), plist::Value::String(value.clone())))
                    .collect(),
            );
        }

        let (events, receiver) = mpsc::unbounded_channel();
        let runner_provider = provider.clone();
        let runner_shared_rsd = shared_rsd.clone();
        let target_name = spec.runner_bundle_id.clone();
        let runner = tokio::spawn(async move {
            let service = xctest_service(runner_provider, runner_shared_rsd);
            let mut listener = RustIosResultListener::new(0, target_name, events);
            service.run(config, &mut listener, None).await
        });
        let mut runner = AbortTaskOnDrop::new(runner);
        let ports = idevice::wda::WdaPorts::default();
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(spec.readiness_timeout_ms);
        let mut last_error = None;
        let status = loop {
            if runner.is_finished() {
                return Err(map_wda_runner_result(runner.take().await));
            }
            let client = idevice::wda::WdaClient::new(&*provider).with_ports(ports);
            match client.status().await {
                Ok(status) => break status,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => {
                    return Err(CoreError::Timeout(format!(
                        "WDA did not become ready within {} ms: {}",
                        spec.readiness_timeout_ms,
                        last_error.as_ref().unwrap_or(&error)
                    )));
                }
            }
        };
        let bridge = idevice::wda_bridge::WdaBridge::start_with_ports(provider.clone(), ports)
            .await
            .map_err(map_device_error)?;
        let endpoints = bridge.endpoints().clone();
        Ok(Box::new(IdeviceWdaSession {
            info: WdaSessionInfo {
                udid,
                wda_url: endpoints.wda_url,
                mjpeg_url: endpoints.mjpeg_url,
                device_http_port: endpoints.device_ports.http,
                device_mjpeg_port: endpoints.device_ports.mjpeg,
                status,
                service_transport,
            },
            provider,
            ports,
            events: receiver,
            runner: Some(runner.take()),
            bridge: Some(bridge),
        }))
    }

    async fn start_ui_runner_session(
        &self,
        spec: &UiRunnerSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn UiRunnerSession>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let udid = device.udid.clone();
        let (shared_rsd, service_transport) = shared_xctest_rsd(tunnel, &udid)?;
        let provider: Arc<dyn IdeviceProvider> =
            Arc::new(device.to_provider(self.addr.clone(), "rust-ios-ui-runner"));
        let mut install = idevice::installation_proxy::InstallationProxyClient::connect(&*provider)
            .await
            .map_err(map_device_error)?;
        let config = idevice::dvt::xctest::TestConfig::from_installation_proxy(
            &mut install,
            &spec.runner_bundle_id,
            Some(&spec.target_bundle_id),
        )
        .await
        .map_err(map_device_error)?;
        let requested = spec
            .xctest_config
            .strip_suffix(".xctest")
            .unwrap_or(&spec.xctest_config);
        if requested != config.config_name() {
            return Err(CoreError::InvalidArgument(format!(
                "UI runner config {:?} does not match installed runner config {:?}",
                spec.xctest_config,
                format!("{}.xctest", config.config_name())
            )));
        }

        let (events, receiver) = mpsc::unbounded_channel();
        let runner_provider = provider.clone();
        let runner_shared_rsd = shared_rsd.clone();
        let target_name = spec.target.as_str().to_owned();
        let runner = tokio::spawn(async move {
            let service = xctest_service(runner_provider, runner_shared_rsd);
            let mut listener = RustIosResultListener::new(0, target_name, events);
            service.run(config, &mut listener, None).await
        });
        Ok(Box::new(IdeviceUiRunnerSession {
            info: UiRunnerSessionInfo {
                udid,
                service_transport,
            },
            events: receiver,
            runner: Some(runner),
        }))
    }

    async fn start_xctest_session(
        &self,
        spec: &XCTestSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn XCTestSession>> {
        if spec.targets.is_empty() {
            return Err(CoreError::InvalidArgument(
                "XCTest requires at least one target".into(),
            ));
        }
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let udid = device.udid.clone();
        let (shared_rsd, service_transport) = shared_xctest_rsd(tunnel, &udid)?;
        let provider: Arc<dyn IdeviceProvider> =
            Arc::new(device.to_provider(self.addr.clone(), "rust-ios-xctest"));
        let (events, receiver) = mpsc::unbounded_channel();
        let run_spec = spec.clone();
        let runner_provider = provider.clone();
        let runner_shared_rsd = shared_rsd.clone();
        let runner = tokio::spawn(async move {
            run_xctest_targets(runner_provider, runner_shared_rsd, run_spec, events).await
        });
        Ok(Box::new(IdeviceXCTestSession {
            info: XCTestSessionInfo {
                udid,
                target_count: spec.targets.len(),
                service_transport,
            },
            events: receiver,
            runner: Some(runner),
        }))
    }

    async fn start_app_process_session(
        &self,
        spec: &AppLaunchSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn AppProcessSession>> {
        let tunnel = tunnel.ok_or_else(|| {
            CoreError::DeviceUnavailable(
                "launch --wait requires a running server-owned userspace tunnel; run tunnel start --userspace"
                    .into(),
            )
        })?;
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        if tunnel.info().udid != device.udid {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }

        let stdio_stream = tunnel
            .connect_rsd_service(RsdServiceKind::OpenStdio)
            .await?;
        let stdio_stream: Box<dyn idevice::ReadWrite> = Box::new(stdio_stream);
        let mut stdio = idevice::core_device::OpenStdioSocketClient::from_stream(stdio_stream)
            .await
            .map_err(map_device_error)?;
        let stdio_uuid = stdio.read_uuid().await.map_err(map_device_error)?;

        let app_stream = tunnel
            .connect_rsd_service(RsdServiceKind::AppService)
            .await?;
        let mut app = idevice::core_device::AppServiceClient::new(app_stream)
            .await
            .map_err(map_device_error)?;
        let arguments = spec
            .arguments
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let environment = (!spec.environment.is_empty()).then(|| {
            spec.environment
                .iter()
                .map(|(key, value)| (key.clone(), plist::Value::String(value.clone())))
                .collect()
        });
        let launched = app
            .launch_application(
                spec.bundle_id.clone(),
                &arguments,
                spec.kill_existing,
                false,
                environment,
                None,
                Some(stdio_uuid),
            )
            .await
            .map_err(map_device_error)?;

        Ok(Box::new(IdeviceAppProcessSession {
            info: AppProcessSessionInfo {
                udid: device.udid,
                bundle_id: spec.bundle_id.clone(),
                pid: launched.pid,
                service_transport: ServiceTransport::SharedRsd,
            },
            stdio,
            app: Some(app),
        }))
    }

    async fn start_webinspector_cdp_session(
        &self,
        spec: &WebInspectorCdpSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorCdpSession>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        if let Some(tunnel) = tunnel.as_ref()
            && tunnel.info().udid != device.udid
        {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }
        let provider = device.to_provider(self.addr.clone(), "rust-ios-webinspector-cdp");
        crate::webinspector::start_cdp_session(device.udid, &provider, spec, tunnel.as_deref())
            .await
    }

    async fn start_webinspector_js_shell_session(
        &self,
        spec: &WebInspectorJsShellSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorJsShellSession>> {
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        if let Some(tunnel) = tunnel.as_ref()
            && tunnel.info().udid != device.udid
        {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }
        let provider = device.to_provider(self.addr.clone(), "rust-ios-webinspector-js-shell");
        crate::webinspector::start_js_shell_session(device.udid, &provider, spec, tunnel.as_deref())
            .await
    }

    async fn execute(&self, invocation: &CommandInvocation) -> CoreResult<CommandResult> {
        self.execute_with_tunnel(invocation, None).await
    }

    async fn execute_with_tunnel(
        &self,
        invocation: &CommandInvocation,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<CommandResult> {
        let command = invocation.argv.first().map(String::as_str).unwrap_or("");
        if command.is_empty() {
            return Err(CoreError::InvalidArgument(
                "empty command invocation".into(),
            ));
        }
        if command == "prepare"
            && let Some(value) = command_prepare_host(invocation).await?
        {
            return Ok(CommandResult {
                exit_code: 0,
                value: Some(value),
                text: None,
                warnings: Vec::new(),
            });
        }
        if command == "ui" && invocation.argv.get(1).map(String::as_str) != Some("install") {
            return Ok(CommandResult {
                exit_code: 0,
                value: Some(crate::ui::execute(invocation).await?),
                text: None,
                warnings: Vec::new(),
            });
        }
        let prepared_wallpaper = if command == "set-wallpaper" {
            Some(prepare_set_wallpaper(invocation).await?)
        } else {
            None
        };
        let prepared_pair = if command == "pair" {
            Some(prepare_pair(invocation).await?)
        } else {
            None
        };
        let target = DeviceTarget {
            udid: invocation.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = self.raw_device(&target).await?;
        let provider = device.to_provider(self.addr.clone(), "rust-ios-command");
        let mut warnings = Vec::new();
        if tunnel.is_some() && command_uses_shared_rsd(invocation) {
            warnings.push("reused server-owned shared RSD tunnel".into());
        }

        let mut text = None;
        let value = match command {
            "activate" => {
                command_activate(&provider, invocation, &device.udid, &mut warnings).await?
            }
            "erase" => command_erase(&provider, invocation, &device.udid).await?,
            "set-wallpaper" => {
                command_set_wallpaper(
                    &provider,
                    &device.udid,
                    prepared_wallpaper.expect("set-wallpaper was prepared"),
                    &mut warnings,
                )
                .await?
            }
            "ui" => {
                if invocation.argv.get(1).map(String::as_str) != Some("install") {
                    return Err(CoreError::InvalidArgument(
                        "ui device operation requires the install subcommand".into(),
                    ));
                }
                let prepared = crate::ui::prepare_install(invocation, Some(&device.udid)).await?;
                idevice::utils::installation::install_package(
                    &provider,
                    &prepared.signed.output_path,
                    None,
                )
                .await
                .map_err(map_device_error)?;
                json!({
                    "status": "complete",
                    "target": prepared.target,
                    "artifactPath": prepared.artifact_path,
                    "appPath": prepared.signed.output_path,
                    "bundleID": prepared.signed.bundle_id,
                    "udid": device.udid,
                })
            }
            "sign" => {
                if invocation.argv.get(1).map(String::as_str) != Some("app")
                    || !has_flag(&invocation.argv, "--install")
                {
                    return Err(CoreError::NotImplemented(
                        "only sign app --install is a device operation; host-only sign app is handled before device selection"
                            .into(),
                    ));
                }
                let signed =
                    crate::signing::prepare_sign_app(invocation, Some(&device.udid)).await?;
                idevice::utils::installation::install_package(&provider, &signed.output_path, None)
                    .await
                    .map_err(map_device_error)?;
                json!({
                    "status": "complete",
                    "appPath": signed.output_path,
                    "bundleID": signed.bundle_id,
                    "udid": device.udid,
                })
            }
            "pair" => match prepared_pair.expect("pair was prepared") {
                PreparedPair::Standard { host_name } => {
                    let mut mux = self
                        .addr
                        .connect(self.tag())
                        .await
                        .map_err(map_usbmuxd_error)?;
                    let buid = mux.get_buid().await.map_err(map_usbmuxd_error)?;
                    let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
                    let mut lockdown = idevice::lockdown::LockdownClient::connect(&provider)
                        .await
                        .map_err(map_device_error)?;
                    let mut pairing = lockdown
                        .pair(host_id.clone(), buid.clone(), host_name.as_deref())
                        .await
                        .map_err(map_device_error)?;
                    lockdown
                        .start_session(&pairing)
                        .await
                        .map_err(map_device_error)?;
                    pairing.udid = Some(device.udid.clone());
                    let bytes = pairing.serialize().map_err(map_device_error)?;
                    mux.save_pair_record(&device.udid, bytes)
                        .await
                        .map_err(map_usbmuxd_error)?;
                    json!({
                        "udid": device.udid,
                        "hostID": host_id,
                        "systemBUID": buid,
                        "saved": true,
                        "supervised": false,
                    })
                }
                PreparedPair::Supervised { p12, password } => {
                    let mut mux = self
                        .addr
                        .connect(self.tag())
                        .await
                        .map_err(map_usbmuxd_error)?;
                    let buid = mux.get_buid().await.map_err(map_usbmuxd_error)?;
                    let idevice = provider
                        .connect(idevice::lockdown::LockdownClient::LOCKDOWND_PORT)
                        .await
                        .map_err(map_device_error)?;
                    let mut stream = idevice.get_socket().ok_or_else(|| {
                        CoreError::DeviceUnavailable(
                            "raw Lockdown connection did not contain a stream".into(),
                        )
                    })?;
                    let result = tokio::time::timeout(
                            Duration::from_secs(45),
                            crate::supervised_pair::pair_supervised(
                                &mut stream,
                                &p12,
                                &password,
                                &buid,
                                &device.udid,
                            ),
                        )
                        .await
                        .map_err(|_| {
                            CoreError::Timeout(
                                "supervised pair exceeded 45 seconds; pairing state is unknown and no host record was saved"
                                    .into(),
                            )
                        })??;
                    mux.save_pair_record(&device.udid, result.pair_record)
                        .await
                        .map_err(map_usbmuxd_error)?;
                    json!({
                        "udid": device.udid,
                        "hostID": result.host_id,
                        "systemBUID": buid,
                        "saved": true,
                        "supervised": true,
                    })
                }
            },
            "info" => match positional_value(&invocation.argv, 1).as_deref() {
                Some("display") => command_display_info(tunnel.as_deref()).await?,
                None | Some("lockdown") => {
                    let mut client = idevice::lockdown::LockdownClient::connect(&provider)
                        .await
                        .map_err(map_device_error)?;
                    if let Ok(pairing) = provider.get_pairing_file().await {
                        let _ = client.start_session(&pairing).await;
                    }
                    plist_json(
                        client
                            .get_value(None, None)
                            .await
                            .map_err(map_device_error)?,
                    )?
                }
                Some(action) => {
                    return Err(CoreError::InvalidArgument(format!(
                        "info accepts display or lockdown, got {action:?}"
                    )));
                }
            },
            "lockdown" => {
                if invocation.argv.get(1).map(String::as_str) != Some("get") {
                    return Err(CoreError::NotImplemented(
                        "only lockdown get is implemented in v0.1".into(),
                    ));
                }
                let key = invocation
                    .argv
                    .get(2)
                    .filter(|value| !value.starts_with('-'));
                let domain = option_value(&invocation.argv, "--domain");
                let mut client = idevice::lockdown::LockdownClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                if let Ok(pairing) = provider.get_pairing_file().await {
                    let _ = client.start_session(&pairing).await;
                }
                plist_json(
                    client
                        .get_value(key.map(String::as_str), domain.as_deref())
                        .await
                        .map_err(map_device_error)?,
                )?
            }
            "devicename" => {
                let mut client = idevice::lockdown::LockdownClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                let value = client
                    .get_value(Some("DeviceName"), None)
                    .await
                    .map_err(map_device_error)?;
                json!({ "DeviceName": plist_json(value)? })
            }
            "date" => {
                let (value, formatted) = command_device_date(&provider).await?;
                text = Some(formatted);
                value
            }
            "lang" => command_language(&provider, invocation).await?,
            "assistivetouch" | "voiceover" | "zoom" | "timeformat" => {
                command_lockdown_toggle(&provider, invocation).await?
            }
            "ip" => command_ip(&provider, invocation).await?,
            "devicestate" => {
                command_device_state_list(&provider, invocation, tunnel.as_deref()).await?
            }
            "webinspector" => {
                crate::webinspector::execute(&provider, invocation, tunnel.as_deref()).await?
            }
            "ax" | "resetax" => crate::accessibility::execute(&provider, invocation).await?,
            "apps" => {
                let application_type = if has_flag(&invocation.argv, "--system") {
                    "System"
                } else if has_flag(&invocation.argv, "--all") {
                    "Any"
                } else {
                    "User"
                };
                let mut client =
                    idevice::installation_proxy::InstallationProxyClient::connect(&provider)
                        .await
                        .map_err(map_device_error)?;
                let mut apps = client
                    .get_apps(Some(application_type), None)
                    .await
                    .map_err(map_device_error)?;
                if has_flag(&invocation.argv, "--filesharing") {
                    apps.retain(|_, app| {
                        app.as_dictionary()
                            .and_then(|dictionary| dictionary.get("UIFileSharingEnabled"))
                            .and_then(plist::Value::as_boolean)
                            .unwrap_or(false)
                    });
                }
                if has_flag(&invocation.argv, "--list") {
                    let mut rows = Vec::with_capacity(apps.len());
                    for (bundle_id, app) in apps {
                        let dictionary = app.as_dictionary();
                        rows.push(json!({
                            "CFBundleIdentifier": bundle_id,
                            "CFBundleDisplayName": plist_string(dictionary, "CFBundleDisplayName")
                                .or_else(|| plist_string(dictionary, "CFBundleName")),
                            "CFBundleShortVersionString": plist_string(dictionary, "CFBundleShortVersionString"),
                        }));
                    }
                    Value::Array(rows)
                } else {
                    plist_json(apps)?
                }
            }
            "debug" => command_debug_app_info(&provider, invocation).await?,
            "diskspace" => {
                let mut afc = idevice::afc::AfcClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                let info = afc.get_device_info().await.map_err(map_device_error)?;
                let used_bytes =
                    info.total_bytes
                        .checked_sub(info.free_bytes)
                        .ok_or_else(|| {
                            CoreError::Backend(format!(
                                "AFC reported free bytes {} above total bytes {}",
                                info.free_bytes, info.total_bytes
                            ))
                        })?;
                json!({
                    "Model": info.model,
                    "BlockSize": info.block_size,
                    "FreeBytes": info.free_bytes,
                    "UsedBytes": used_bytes,
                    "TotalBytes": info.total_bytes,
                })
            }
            "crash" => command_crash(&provider, invocation).await?,
            "profile" => command_profile(&provider, invocation, &mut warnings).await?,
            "mdm" => command_mdm(&provider, invocation).await?,
            "wifi" => command_wifi(&provider, invocation, &mut warnings).await?,
            "httpproxy" => command_http_proxy(&provider, invocation).await?,
            "prepare" => command_prepare(&provider, invocation, &mut warnings).await?,
            "fsync" => command_fsync(&provider, invocation).await?,
            "file" => command_file(&provider, invocation, tunnel.as_deref()).await?,
            "pasteboard" => command_pasteboard(invocation, tunnel.as_deref()).await?,
            "install" => {
                let raw_path = option_value(&invocation.argv, "--path")
                    .or_else(|| invocation.argv.get(1).cloned())
                    .ok_or_else(|| CoreError::InvalidArgument("install requires --path".into()))?;
                let path = resolve_client_path(invocation, &raw_path);
                idevice::utils::installation::install_package(&provider, &path, None)
                    .await
                    .map_err(map_device_error)?;
                json!({ "status": "complete", "path": path })
            }
            "uninstall" => {
                let bundle_id = invocation
                    .argv
                    .get(1)
                    .filter(|value| !value.starts_with('-'))
                    .ok_or_else(|| {
                        CoreError::InvalidArgument("uninstall requires bundle ID".into())
                    })?;
                let mut client =
                    idevice::installation_proxy::InstallationProxyClient::connect(&provider)
                        .await
                        .map_err(map_device_error)?;
                client
                    .uninstall(bundle_id.clone(), None)
                    .await
                    .map_err(map_device_error)?;
                json!({ "status": "complete", "bundleID": bundle_id })
            }
            "devmode" => {
                let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
                    CoreError::InvalidArgument("devmode requires enable, get or reveal".into())
                })?;
                let mut client = idevice::amfi::AmfiClient::connect(&provider)
                    .await
                    .map_err(map_device_error)?;
                match action {
                    "get" => json!({
                        "DeveloperModeEnabled": client
                            .get_developer_mode_status()
                            .await
                            .map_err(map_device_error)?
                    }),
                    "reveal" => {
                        client
                            .reveal_developer_mode_option_in_ui()
                            .await
                            .map_err(map_device_error)?;
                        json!({ "status": "revealed" })
                    }
                    "enable" if has_flag(&invocation.argv, "--enable-post-restart") => {
                        client
                            .accept_developer_mode()
                            .await
                            .map_err(map_device_error)?;
                        json!({ "status": "accepted_post_restart" })
                    }
                    "enable" => {
                        client
                            .enable_developer_mode()
                            .await
                            .map_err(map_device_error)?;
                        json!({ "status": "restarting" })
                    }
                    _ => {
                        return Err(CoreError::InvalidArgument(format!(
                            "unknown devmode action {action:?}"
                        )));
                    }
                }
            }
            "image" => command_image(&provider, invocation).await?,
            "ps" => {
                let mut remote = connect_dvt(&provider, tunnel.as_deref()).await?;
                let mut service = idevice::dvt::device_info::DeviceInfoClient::new(&mut remote)
                    .await
                    .map_err(map_device_error)?;
                let applications_only = has_flag(&invocation.argv, "--apps");
                let processes = service
                    .running_processes()
                    .await
                    .map_err(map_device_error)?
                    .into_iter()
                    .filter(|process| !applications_only || process.is_application)
                    .map(|process| {
                        json!({
                            "pid": process.pid,
                            "name": process.name,
                            "realAppName": process.real_app_name,
                            "isApplication": process.is_application,
                            "startPageCount": process.start_page_count,
                        })
                    })
                    .collect::<Vec<_>>();
                Value::Array(processes)
            }
            "launch" => {
                if launch_flag(&invocation.argv, "--wait") {
                    return Err(CoreError::NotImplemented(
                        "launch --wait requires a server-owned session task and is not implemented"
                            .into(),
                    ));
                }
                let bundle_id = positional_value(&invocation.argv, 1).ok_or_else(|| {
                    CoreError::InvalidArgument("launch requires a bundle ID".into())
                })?;
                let environment = parse_key_value_options(&invocation.argv, "--env")?;
                let mut arguments = plist::Dictionary::new();
                for (index, argument) in option_values(&invocation.argv, "--arg")
                    .into_iter()
                    .enumerate()
                {
                    arguments.insert(index.to_string(), plist::Value::String(argument));
                }
                let mut remote = connect_dvt(&provider, tunnel.as_deref()).await?;
                let mut process =
                    idevice::dvt::process_control::ProcessControlClient::new(&mut remote)
                        .await
                        .map_err(map_device_error)?;
                let pid = process
                    .launch_app(
                        bundle_id.clone(),
                        (!environment.is_empty()).then_some(environment),
                        (!arguments.is_empty()).then_some(arguments),
                        false,
                        launch_flag(&invocation.argv, "--kill-existing"),
                    )
                    .await
                    .map_err(map_device_error)?;
                json!({ "bundleID": bundle_id, "pid": pid })
            }
            "kill" | "memlimitoff" => {
                command_process_mutation(&provider, invocation, tunnel.as_deref()).await?
            }
            "diagnostics" | "batterycheck" | "batteryregistry" | "mobilegestalt" | "reboot"
            | "shutdown" => {
                let mut client =
                    idevice::diagnostics_relay::DiagnosticsRelayClient::connect(&provider)
                        .await
                        .map_err(map_device_error)?;
                match command {
                    "diagnostics" => {
                        validate_diagnostics_list(&invocation.argv)?;
                        diagnostics_list_response(client.all().await.map_err(map_device_error)?)?
                    }
                    "batterycheck" => optional_dictionary_json(
                        client.gasguage().await.map_err(map_device_error)?,
                    )?,
                    "batteryregistry" => optional_dictionary_json(
                        client
                            .ioregistry(None, None, Some("IOPMPowerSource"))
                            .await
                            .map_err(map_device_error)?,
                    )?,
                    "mobilegestalt" => {
                        let keys: Vec<String> = invocation
                            .argv
                            .iter()
                            .skip(1)
                            .filter(|value| !value.starts_with('-'))
                            .cloned()
                            .collect();
                        let response = client
                            .mobilegestalt((!keys.is_empty()).then_some(keys))
                            .await
                            .map_err(map_device_error)?
                            .ok_or_else(|| {
                                CoreError::Backend(
                                    "mobilegestalt response did not contain a Diagnostics dictionary"
                                        .into(),
                                )
                            })?;
                        let response = crate::mobilegestalt_envelope(response);
                        if has_flag(&invocation.argv, "--plist") {
                            return Ok(CommandResult {
                                exit_code: 0,
                                value: None,
                                text: Some(crate::plist_dictionary_xml(&response)?),
                                warnings,
                            });
                        }
                        plist_json(response)?
                    }
                    "reboot" => {
                        client.restart().await.map_err(map_device_error)?;
                        json!({ "status": "restarting" })
                    }
                    "shutdown" => {
                        client.shutdown().await.map_err(map_device_error)?;
                        json!({ "status": "shutting_down" })
                    }
                    _ => unreachable!("diagnostics relay match only contains known commands"),
                }
            }
            "setlocation" => {
                let raw_latitude = option_value(&invocation.argv, "--lat").ok_or_else(|| {
                    CoreError::InvalidArgument("setlocation requires --lat".into())
                })?;
                let raw_longitude = option_value(&invocation.argv, "--lon").ok_or_else(|| {
                    CoreError::InvalidArgument("setlocation requires --lon".into())
                })?;
                let latitude = parse_location_coordinate(&raw_latitude, -90.0, 90.0, "latitude")?;
                let longitude =
                    parse_location_coordinate(&raw_longitude, -180.0, 180.0, "longitude")?;
                let spec = one_shot_location_spec(invocation);
                let mut session = self
                    .start_location_simulation(&spec, tunnel.clone())
                    .await?;
                let service_transport = session.info().service_transport.clone();
                session.set_location(latitude, longitude).await?;
                session.close().await?;
                json!({
                    "status": "set",
                    "latitude": latitude,
                    "longitude": longitude,
                    "serviceTransport": service_transport,
                })
            }
            "resetlocation" => {
                let spec = one_shot_location_spec(invocation);
                let mut session = self
                    .start_location_simulation(&spec, tunnel.clone())
                    .await?;
                let service_transport = session.info().service_transport.clone();
                session.clear().await?;
                session.close().await?;
                json!({ "status": "cleared", "serviceTransport": service_transport })
            }
            "get-app-icon" | "get-wallpaper" | "get-icon-layout" | "set-icon-layout" => {
                command_springboard(&provider, invocation).await?
            }
            "screenshot" => {
                if has_flag(&invocation.argv, "--stream")
                    || option_value(&invocation.argv, "--port").is_some()
                {
                    return Err(CoreError::NotImplemented(
                        "screenshot MJPEG streaming requires a server stream task and is not implemented"
                            .into(),
                    ));
                }
                let mut remote = connect_dvt(&provider, tunnel.as_deref()).await?;
                let mut client = idevice::dvt::screenshot::ScreenshotClient::new(&mut remote)
                    .await
                    .map_err(map_device_error)?;
                let bytes = client.take_screenshot().await.map_err(map_device_error)?;
                let raw_output = option_value(&invocation.argv, "--output")
                    .unwrap_or_else(|| "screenshot.png".into());
                let output = resolve_client_path(invocation, &raw_output);
                if let Some(parent) = output.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&output, &bytes).await?;
                json!({ "output": output, "bytes": bytes.len() })
            }
            _ => {
                return Err(CoreError::NotImplemented(format!(
                    "command {command:?} is registered but has not been migrated yet"
                )));
            }
        };

        Ok(CommandResult {
            exit_code: 0,
            value: Some(value),
            text,
            warnings,
        })
    }
}

#[derive(Debug)]
struct IdeviceUserSpaceTunnel {
    info: UserSpaceTunnelInfo,
    adapter: Mutex<Option<AdapterHandle>>,
    rsd: Mutex<Option<idevice::rsd::RsdHandshake>>,
}

#[cfg(windows)]
#[derive(Debug)]
struct IdeviceKernelRouteTunnel {
    info: UserSpaceTunnelInfo,
    rsd: Mutex<Option<idevice::rsd::RsdHandshake>>,
    closed: AtomicBool,
}

#[cfg(windows)]
struct IdeviceKernelTunnelRunner {
    device: Option<Arc<tun_rs::AsyncDevice>>,
    reader: Option<tokio::io::ReadHalf<Box<dyn idevice::ReadWrite>>>,
    writer: Option<tokio::io::WriteHalf<Box<dyn idevice::ReadWrite>>>,
    mtu: usize,
    closed: bool,
}

#[cfg(windows)]
impl std::fmt::Debug for IdeviceKernelTunnelRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdeviceKernelTunnelRunner")
            .field("device_open", &self.device.is_some())
            .field("reader_open", &self.reader.is_some())
            .field("writer_open", &self.writer.is_some())
            .field("mtu", &self.mtu)
            .field("closed", &self.closed)
            .finish()
    }
}

#[derive(Debug)]
struct IdeviceCondition {
    info: DeviceConditionInfo,
    remote: Mutex<Option<DvtClient>>,
}

#[derive(Debug)]
struct IdeviceLocationSimulation {
    info: LocationSimulationInfo,
    transport: Option<IdeviceLocationTransport>,
}

#[derive(Debug)]
enum IdeviceLocationTransport {
    Dvt(DvtClient),
    Classic(UsbmuxdProvider),
}

#[derive(Debug, Default)]
struct XCTestTargetCounts {
    suite_count: u64,
    test_count: u64,
    passed: u64,
    failed: u64,
    skipped: u64,
    expected_failures: u64,
    stalled: u64,
}

struct RustIosResultListener {
    target_index: usize,
    target_name: String,
    events: mpsc::UnboundedSender<XCTestEvent>,
    counts: XCTestTargetCounts,
}

impl RustIosResultListener {
    fn new(
        target_index: usize,
        target_name: String,
        events: mpsc::UnboundedSender<XCTestEvent>,
    ) -> Self {
        Self {
            target_index,
            target_name,
            events,
            counts: XCTestTargetCounts::default(),
        }
    }

    fn emit(&self, event: XCTestEventKind) {
        let _ = self.events.send(XCTestEvent {
            timestamp_ms: xctest_unix_time_ms(),
            target_index: self.target_index,
            target_name: self.target_name.clone(),
            event,
        });
    }
}

impl idevice::dvt::xctest::listener::XCUITestListener for RustIosResultListener {
    async fn did_begin_executing_test_plan(&mut self) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::TestPlanStarted);
        Ok(())
    }

    async fn did_finish_executing_test_plan(&mut self) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::TestPlanFinished);
        Ok(())
    }

    async fn test_suite_did_start(
        &mut self,
        suite: &str,
        started_at: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::SuiteStarted {
            suite: suite.into(),
            started_at: started_at.into(),
        });
        Ok(())
    }

    async fn test_suite_did_finish(
        &mut self,
        suite: &str,
        finished_at: &str,
        run_count: u64,
        failures: u64,
        unexpected: u64,
        test_duration: f64,
        total_duration: f64,
        skipped: u64,
        expected_failures: u64,
        uncaught_exceptions: u64,
    ) -> Result<(), idevice::IdeviceError> {
        self.counts.suite_count = self.counts.suite_count.saturating_add(1);
        self.emit(XCTestEventKind::SuiteFinished {
            suite: suite.into(),
            finished_at: finished_at.into(),
            run_count,
            failures,
            unexpected,
            test_duration,
            total_duration,
            skipped,
            expected_failures,
            uncaught_exceptions,
        });
        Ok(())
    }

    async fn test_case_did_start(
        &mut self,
        test_class: &str,
        method: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::CaseStarted {
            test_class: test_class.into(),
            method: method.into(),
        });
        Ok(())
    }

    async fn test_case_did_finish(
        &mut self,
        result: idevice::dvt::xctest::listener::XCTestCaseResult,
    ) -> Result<(), idevice::IdeviceError> {
        self.counts.test_count = self.counts.test_count.saturating_add(1);
        match result.status.to_ascii_lowercase().as_str() {
            "passed" => self.counts.passed = self.counts.passed.saturating_add(1),
            "skipped" => self.counts.skipped = self.counts.skipped.saturating_add(1),
            "expected failure" | "expected_failure" => {
                self.counts.expected_failures = self.counts.expected_failures.saturating_add(1)
            }
            _ => self.counts.failed = self.counts.failed.saturating_add(1),
        }
        self.emit(XCTestEventKind::CaseFinished {
            test_class: result.test_class,
            method: result.method,
            status: result.status,
            duration: result.duration,
        });
        Ok(())
    }

    async fn test_case_did_fail(
        &mut self,
        test_class: &str,
        method: &str,
        message: &str,
        file: &str,
        line: u64,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::CaseFailed {
            test_class: test_class.into(),
            method: method.into(),
            message: message.into(),
            file: file.into(),
            line,
        });
        Ok(())
    }

    async fn test_case_did_stall(
        &mut self,
        test_class: &str,
        method: &str,
        file: &str,
        line: u64,
    ) -> Result<(), idevice::IdeviceError> {
        self.counts.stalled = self.counts.stalled.saturating_add(1);
        self.emit(XCTestEventKind::CaseStalled {
            test_class: test_class.into(),
            method: method.into(),
            file: file.into(),
            line,
        });
        Ok(())
    }

    async fn test_case_will_start_activity(
        &mut self,
        test_class: &str,
        method: &str,
        activity_title: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::ActivityStarted {
            test_class: test_class.into(),
            method: method.into(),
            title: activity_title.into(),
        });
        Ok(())
    }

    async fn test_case_did_finish_activity(
        &mut self,
        test_class: &str,
        method: &str,
        activity_title: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::ActivityFinished {
            test_class: test_class.into(),
            method: method.into(),
            title: activity_title.into(),
        });
        Ok(())
    }

    async fn test_method_did_measure_metric(
        &mut self,
        test_class: &str,
        method: &str,
        metric: &str,
        file: &str,
        line: u64,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::Metric {
            test_class: test_class.into(),
            method: method.into(),
            metric: metric.into(),
            file: file.into(),
            line,
        });
        Ok(())
    }

    async fn log_message(&mut self, message: &str) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::Log {
            message: message.into(),
            debug: false,
        });
        Ok(())
    }

    async fn log_debug_message(&mut self, message: &str) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::Log {
            message: message.into(),
            debug: true,
        });
        Ok(())
    }

    async fn exchange_protocol_version(
        &mut self,
        current: u64,
        minimum: u64,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::ProtocolVersion { current, minimum });
        Ok(())
    }

    async fn did_begin_initializing_for_ui_testing(&mut self) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::UiInitializationStarted);
        Ok(())
    }

    async fn initialization_for_ui_testing_did_fail(
        &mut self,
        description: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::UiInitializationFailed {
            description: description.into(),
        });
        Err(idevice::IdeviceError::UnexpectedResponse(format!(
            "UI testing initialization failed: {description}"
        )))
    }

    async fn did_fail_to_bootstrap(
        &mut self,
        description: &str,
    ) -> Result<(), idevice::IdeviceError> {
        self.emit(XCTestEventKind::BootstrapFailed {
            description: description.into(),
        });
        Err(idevice::IdeviceError::UnexpectedResponse(format!(
            "test runner failed to bootstrap: {description}"
        )))
    }
}

async fn run_xctest_targets(
    provider: Arc<dyn IdeviceProvider>,
    shared_rsd: Option<Arc<dyn idevice::dvt::xctest::XCTestRsdProvider>>,
    spec: XCTestSpec,
    events: mpsc::UnboundedSender<XCTestEvent>,
) -> CoreResult<XCTestRunSummary> {
    let started_at_ms = xctest_unix_time_ms();
    let mut summary = XCTestRunSummary {
        started_at_ms,
        finished_at_ms: started_at_ms,
        duration_ms: 0,
        target_count: spec.targets.len(),
        completed_targets: 0,
        suite_count: 0,
        test_count: 0,
        passed: 0,
        failed: 0,
        skipped: 0,
        expected_failures: 0,
        stalled: 0,
    };
    for (target_index, target) in spec.targets.iter().enumerate() {
        emit_xctest_event(
            &events,
            target_index,
            &target.name,
            XCTestEventKind::TargetStarted,
        );
        let result = run_xctest_target(
            provider.clone(),
            shared_rsd.clone(),
            target_index,
            target,
            spec.timeout_ms.map(Duration::from_millis),
            events.clone(),
        )
        .await;
        let counts = match result {
            Ok(counts) => counts,
            Err(error) => {
                emit_xctest_event(
                    &events,
                    target_index,
                    &target.name,
                    XCTestEventKind::TargetFinished {
                        outcome: format!("error: {error}"),
                    },
                );
                return Err(error);
            }
        };
        let outcome = if counts.failed > 0 || counts.stalled > 0 {
            "failed"
        } else {
            "passed"
        };
        emit_xctest_event(
            &events,
            target_index,
            &target.name,
            XCTestEventKind::TargetFinished {
                outcome: outcome.into(),
            },
        );
        summary.completed_targets = summary.completed_targets.saturating_add(1);
        summary.suite_count = summary.suite_count.saturating_add(counts.suite_count);
        summary.test_count = summary.test_count.saturating_add(counts.test_count);
        summary.passed = summary.passed.saturating_add(counts.passed);
        summary.failed = summary.failed.saturating_add(counts.failed);
        summary.skipped = summary.skipped.saturating_add(counts.skipped);
        summary.expected_failures = summary
            .expected_failures
            .saturating_add(counts.expected_failures);
        summary.stalled = summary.stalled.saturating_add(counts.stalled);
    }
    summary.finished_at_ms = xctest_unix_time_ms();
    summary.duration_ms = summary.finished_at_ms.saturating_sub(summary.started_at_ms);
    Ok(summary)
}

async fn run_xctest_target(
    provider: Arc<dyn IdeviceProvider>,
    shared_rsd: Option<Arc<dyn idevice::dvt::xctest::XCTestRsdProvider>>,
    target_index: usize,
    target: &XCTestTargetSpec,
    timeout: Option<Duration>,
    events: mpsc::UnboundedSender<XCTestEvent>,
) -> CoreResult<XCTestTargetCounts> {
    let mut install = idevice::installation_proxy::InstallationProxyClient::connect(&*provider)
        .await
        .map_err(map_device_error)?;
    let target_bundle_id = if target.is_ui_test {
        resolve_xctest_target_bundle(&mut install, target).await?
    } else {
        None
    };
    let mut config = idevice::dvt::xctest::TestConfig::from_installation_proxy_with_test_bundle(
        &mut install,
        &target.runner_bundle_id,
        target_bundle_id.as_deref(),
        target.xctest_config.as_deref(),
        target.is_ui_test,
    )
    .await
    .map_err(map_device_error)?;
    if let Some(requested) = target.xctest_config.as_deref() {
        let requested = requested.strip_suffix(".xctest").unwrap_or(requested);
        if requested != config.config_name() {
            return Err(CoreError::InvalidArgument(format!(
                "XCTest target {:?} config {:?} does not match installed runner config {:?}",
                target.name,
                target.xctest_config.as_deref().unwrap_or_default(),
                format!("{}.xctest", config.config_name())
            )));
        }
    }
    if !target.arguments.is_empty() {
        config.runner_args = Some(target.arguments.clone());
    }
    if !target.environment.is_empty() {
        config.runner_env = Some(
            target
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), xctest_environment_value(value)))
                .collect(),
        );
    }
    if !target.tests_to_run.is_empty() {
        config.tests_to_run = Some(target.tests_to_run.clone());
    }
    if !target.tests_to_skip.is_empty() {
        config.tests_to_skip = Some(target.tests_to_skip.clone());
    }

    let mut listener =
        RustIosResultListener::new(target_index, target.name.clone(), events.clone());
    let service = xctest_service(provider, shared_rsd);
    service
        .run(config, &mut listener, timeout)
        .await
        .map_err(map_device_error)?;
    Ok(listener.counts)
}

async fn resolve_xctest_target_bundle(
    install: &mut idevice::installation_proxy::InstallationProxyClient,
    target: &XCTestTargetSpec,
) -> CoreResult<Option<String>> {
    if target.target_bundle_id.is_some() {
        return Ok(target.target_bundle_id.clone());
    }
    let Some(target_app_name) = target.target_app_name.as_deref() else {
        return Ok(None);
    };
    let apps = install
        .get_apps(None, None)
        .await
        .map_err(map_device_error)?;
    for (bundle_id, value) in apps {
        let Some(dictionary) = value.as_dictionary() else {
            continue;
        };
        let name = dictionary
            .get("CFBundleName")
            .or_else(|| dictionary.get("CFBundleDisplayName"))
            .and_then(plist::Value::as_string);
        if name == Some(target_app_name) {
            return Ok(Some(bundle_id));
        }
    }
    Err(CoreError::DeviceNotFound(format!(
        "XCTest target app {target_app_name:?} is not installed"
    )))
}

fn xctest_environment_value(value: &XCTestEnvironmentValue) -> plist::Value {
    match value {
        XCTestEnvironmentValue::String(value) => plist::Value::String(value.clone()),
        XCTestEnvironmentValue::Boolean(value) => plist::Value::Boolean(*value),
        XCTestEnvironmentValue::SignedInteger(value) => {
            plist::Value::Integer(plist::Integer::from(*value))
        }
        XCTestEnvironmentValue::UnsignedInteger(value) => {
            plist::Value::Integer(plist::Integer::from(*value))
        }
        XCTestEnvironmentValue::Real(value) => plist::Value::Real(*value),
    }
}

fn emit_xctest_event(
    events: &mpsc::UnboundedSender<XCTestEvent>,
    target_index: usize,
    target_name: &str,
    event: XCTestEventKind,
) {
    let _ = events.send(XCTestEvent {
        timestamp_ms: xctest_unix_time_ms(),
        target_index,
        target_name: target_name.into(),
        event,
    });
}

fn xctest_unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[derive(Debug)]
struct AbortTaskOnDrop<T> {
    task: Option<JoinHandle<T>>,
}

impl<T> AbortTaskOnDrop<T> {
    fn new(task: JoinHandle<T>) -> Self {
        Self { task: Some(task) }
    }

    fn is_finished(&self) -> bool {
        self.task.as_ref().is_some_and(JoinHandle::is_finished)
    }

    fn take(&mut self) -> JoinHandle<T> {
        self.task.take().expect("WDA runner task is present")
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn shared_xctest_rsd(
    tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    udid: &str,
) -> CoreResult<(
    Option<Arc<dyn idevice::dvt::xctest::XCTestRsdProvider>>,
    ServiceTransport,
)> {
    let Some(tunnel) = tunnel else {
        return Ok((None, ServiceTransport::DedicatedCoreDevice));
    };
    if tunnel.info().udid != udid {
        return Err(CoreError::InvalidArgument(format!(
            "selected tunnel belongs to {}, not {udid}",
            tunnel.info().udid
        )));
    }
    Ok((
        Some(Arc::new(SharedXCTestRsdProvider { tunnel })),
        ServiceTransport::SharedRsd,
    ))
}

fn xctest_service(
    provider: Arc<dyn IdeviceProvider>,
    shared_rsd: Option<Arc<dyn idevice::dvt::xctest::XCTestRsdProvider>>,
) -> idevice::dvt::xctest::XCUITestService {
    let service = idevice::dvt::xctest::XCUITestService::new(provider);
    match shared_rsd {
        Some(shared_rsd) => service.with_shared_rsd_provider(shared_rsd),
        None => service,
    }
}

#[derive(Debug)]
struct SharedXCTestRsdProvider {
    tunnel: Arc<dyn UserSpaceTunnel>,
}

impl idevice::dvt::xctest::XCTestRsdProvider for SharedXCTestRsdProvider {
    fn connect_to_service<'a>(
        &'a self,
        service_name: &'a str,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Box<dyn idevice::ReadWrite>, idevice::IdeviceError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let services = self.tunnel.rsd_services().await.map_err(|error| {
                idevice::IdeviceError::UnknownErrorType(format!(
                    "load shared RSD services for XCTest: {error}"
                ))
            })?;
            let port = services
                .get(service_name)
                .map(|service| service.port)
                .ok_or(idevice::IdeviceError::ServiceNotFound)?;
            let port = u16::try_from(port).map_err(|_| {
                idevice::IdeviceError::UnexpectedResponse(format!(
                    "shared RSD service {service_name:?} has invalid port {port}"
                ))
            })?;
            let stream = self.tunnel.connect(port).await.map_err(|error| {
                idevice::IdeviceError::UnknownErrorType(format!(
                    "connect shared RSD service {service_name:?}: {error}"
                ))
            })?;
            Ok(Box::new(IdeviceSharedRsdSocket(stream)) as Box<dyn idevice::ReadWrite>)
        })
    }
}

#[derive(Debug)]
struct IdeviceSharedRsdSocket(BoxedIo);

impl AsyncRead for IdeviceSharedRsdSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().0).poll_read(context, buffer)
    }
}

impl AsyncWrite for IdeviceSharedRsdSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_write(context, buffer)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_flush(context)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut *self.get_mut().0).poll_shutdown(context)
    }
}

struct IdeviceWdaSession {
    info: WdaSessionInfo,
    provider: Arc<dyn IdeviceProvider>,
    ports: idevice::wda::WdaPorts,
    events: mpsc::UnboundedReceiver<XCTestEvent>,
    runner: Option<JoinHandle<Result<(), idevice::IdeviceError>>>,
    bridge: Option<idevice::wda_bridge::WdaBridge>,
}

#[derive(Debug)]
struct IdeviceUiRunnerSession {
    info: UiRunnerSessionInfo,
    events: mpsc::UnboundedReceiver<XCTestEvent>,
    runner: Option<JoinHandle<Result<(), idevice::IdeviceError>>>,
}

struct IdeviceXCTestSession {
    info: XCTestSessionInfo,
    events: mpsc::UnboundedReceiver<XCTestEvent>,
    runner: Option<JoinHandle<CoreResult<XCTestRunSummary>>>,
}

struct IdeviceAppProcessSession {
    info: AppProcessSessionInfo,
    stdio: idevice::core_device::OpenStdioSocketClient,
    app: Option<idevice::core_device::AppServiceClient<BoxedIo>>,
}

impl std::fmt::Debug for IdeviceAppProcessSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdeviceAppProcessSession")
            .field("info", &self.info)
            .field("app_service_active", &self.app.is_some())
            .finish()
    }
}

impl IdeviceAppProcessSession {
    async fn owned_process_is_running(&mut self) -> CoreResult<bool> {
        let app = self
            .app
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("app process session is closed".into()))?;
        Ok(app
            .list_processes()
            .await
            .map_err(map_device_error)?
            .iter()
            .any(|process| process.pid == self.info.pid))
    }
}

impl std::fmt::Debug for IdeviceXCTestSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdeviceXCTestSession")
            .field("info", &self.info)
            .field("runner_active", &self.runner.is_some())
            .finish()
    }
}

impl std::fmt::Debug for IdeviceWdaSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdeviceWdaSession")
            .field("info", &self.info)
            .field("runner_active", &self.runner.is_some())
            .field("bridge_active", &self.bridge.is_some())
            .finish()
    }
}

#[derive(Debug)]
struct IdeviceSyslogStream {
    client: idevice::syslog_relay::SyslogRelayClient,
    format: StreamFormat,
    info: DeviceStreamInfo,
}

#[async_trait]
impl DeviceStream for IdeviceSyslogStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        let message = self.client.next().await.map_err(map_device_error)?;
        let message = message.trim_end_matches(['\0', '\r', '\n']);
        if self.format == StreamFormat::JsonLines {
            Ok(format!("{}\n", json!({ "message": message })).into_bytes())
        } else {
            Ok(format!("{message}\n").into_bytes())
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct IdevicePcapStream {
    client: idevice::pcapd::PcapdClient,
    pid: Option<u32>,
    process: Option<String>,
    info: DeviceStreamInfo,
}

#[derive(Debug)]
struct IdeviceOsTraceStream {
    receiver: idevice::os_trace_relay::OsTraceRelayReceiver,
    format: StreamFormat,
    options: OsTraceOptions,
    info: DeviceStreamInfo,
}

#[derive(Debug)]
struct IdeviceDvtWorkerStream {
    receiver: mpsc::Receiver<CoreResult<Vec<u8>>>,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    info: DeviceStreamInfo,
}

#[async_trait]
impl DeviceStream for IdeviceDvtWorkerStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        self.receiver.recv().await.unwrap_or_else(|| {
            Err(CoreError::DeviceUnavailable(
                "DVT stream worker exited".into(),
            ))
        })
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.cancel.cancel();
        if let Some(mut join) = self.join.take()
            && tokio::time::timeout(Duration::from_secs(3), &mut join)
                .await
                .is_err()
        {
            join.abort();
            let _ = join.await;
        }
        Ok(())
    }
}

async fn start_notifications_stream(
    provider: UsbmuxdProvider,
    tunnel: Option<Arc<dyn UserSpaceTunnel>>,
) -> CoreResult<Box<dyn DeviceStream>> {
    let service_transport = dvt_service_transport(tunnel.as_ref());
    let (sender, receiver) = mpsc::channel(16);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        let mut remote = match connect_dvt(&provider, tunnel.as_deref()).await {
            Ok(remote) => remote,
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        };
        let mut client =
            match idevice::dvt::notifications::NotificationsClient::new(&mut remote).await {
                Ok(client) => client,
                Err(error) => {
                    let _ = ready_sender.send(Err(map_device_error(error)));
                    return;
                }
            };
        if let Err(error) = client.start_notifications().await {
            let _ = ready_sender.send(Err(map_device_error(error)));
            return;
        }
        if ready_sender.send(Ok(())).is_err() {
            let _ = client.stop_notifications().await;
            return;
        }
        loop {
            tokio::select! {
                () = worker_cancel.cancelled() => {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        client.stop_notifications(),
                    ).await;
                    return;
                }
                result = client.get_notification() => {
                    let result = result
                        .map_err(map_device_error)
                        .and_then(notification_json_line);
                    let failed = result.is_err();
                    if sender.send(result).await.is_err() || failed {
                        return;
                    }
                }
            }
        }
    });
    match ready_receiver.await {
        Ok(Ok(())) => Ok(Box::new(IdeviceDvtWorkerStream {
            receiver,
            cancel,
            join: Some(join),
            info: DeviceStreamInfo {
                content_type: "application/x-ndjson".into(),
                initial_bytes: Vec::new(),
                service_transport,
            },
        })),
        Ok(Err(error)) => {
            let _ = join.await;
            Err(error)
        }
        Err(_) => {
            let _ = join.await;
            Err(CoreError::Backend(
                "notifications worker exited during setup".into(),
            ))
        }
    }
}

async fn start_screenshot_dvt_stream(
    provider: UsbmuxdProvider,
    tunnel: Option<Arc<dyn UserSpaceTunnel>>,
) -> CoreResult<Box<dyn DeviceStream>> {
    let service_transport = dvt_service_transport(tunnel.as_ref());
    let (sender, receiver) = mpsc::channel(2);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        let mut remote = match connect_dvt(&provider, tunnel.as_deref()).await {
            Ok(remote) => remote,
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        };
        let mut client = match idevice::dvt::screenshot::ScreenshotClient::new(&mut remote).await {
            Ok(client) => client,
            Err(error) => {
                let _ = ready_sender.send(Err(map_device_error(error)));
                return;
            }
        };
        if ready_sender.send(Ok(())).is_err() {
            return;
        }
        loop {
            tokio::select! {
                () = worker_cancel.cancelled() => return,
                result = client.take_screenshot() => {
                    let result = match result {
                        Ok(bytes) => tokio::task::spawn_blocking(move || screenshot_to_jpeg(&bytes))
                            .await
                            .map_err(|error| CoreError::Backend(format!("screenshot JPEG worker failed: {error}")))
                            .and_then(|result| result),
                        Err(error) => Err(map_device_error(error)),
                    };
                    let failed = result.is_err();
                    if sender.send(result).await.is_err() || failed {
                        return;
                    }
                }
            }
        }
    });
    match ready_receiver.await {
        Ok(Ok(())) => Ok(Box::new(IdeviceDvtWorkerStream {
            receiver,
            cancel,
            join: Some(join),
            info: DeviceStreamInfo {
                content_type: "image/jpeg".into(),
                initial_bytes: Vec::new(),
                service_transport,
            },
        })),
        Ok(Err(error)) => {
            let _ = join.await;
            Err(error)
        }
        Err(_) => {
            let _ = join.await;
            Err(CoreError::Backend(
                "screenshot worker exited during setup".into(),
            ))
        }
    }
}

fn screenshot_to_jpeg(bytes: &[u8]) -> CoreResult<Vec<u8>> {
    if bytes.starts_with(&[0xff, 0xd8]) && bytes.ends_with(&[0xff, 0xd9]) {
        return Ok(bytes.to_vec());
    }
    let image = image::load_from_memory(bytes)
        .map_err(|error| CoreError::Backend(format!("decode screenshot image: {error}")))?;
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&image)
        .map_err(|error| CoreError::Backend(format!("encode screenshot JPEG: {error}")))?;
    Ok(jpeg)
}

async fn start_sysmontap_stream(
    provider: UsbmuxdProvider,
    tunnel: Option<Arc<dyn UserSpaceTunnel>>,
) -> CoreResult<Box<dyn DeviceStream>> {
    let service_transport = dvt_service_transport(tunnel.as_ref());
    let (sender, receiver) = mpsc::channel(16);
    let (ready_sender, ready_receiver) = oneshot::channel();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let join = tokio::spawn(async move {
        let mut remote = match connect_dvt(&provider, tunnel.as_deref()).await {
            Ok(remote) => remote,
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        };
        let (process_attributes, system_attributes) = match sysmontap_attributes(&mut remote).await
        {
            Ok(attributes) => attributes,
            Err(error) => {
                let _ = ready_sender.send(Err(error));
                return;
            }
        };
        let mut channel = match remote
            .make_channel("com.apple.instruments.server.services.sysmontap")
            .await
        {
            Ok(channel) => channel,
            Err(error) => {
                let _ = ready_sender.send(Err(map_device_error(error)));
                return;
            }
        };
        let config = sysmontap_config(process_attributes, system_attributes);
        if let Err(error) = channel
            .call_method(
                Some(plist::Value::String("setConfig:".into())),
                Some(vec![idevice::dvt::message::AuxValue::archived_value(
                    plist::Value::Dictionary(config),
                )]),
                false,
            )
            .await
        {
            let _ = ready_sender.send(Err(map_device_error(error)));
            return;
        }
        if let Err(error) = channel
            .call_method(Some(plist::Value::String("start".into())), None, false)
            .await
        {
            let _ = ready_sender.send(Err(map_device_error(error)));
            return;
        }
        if let Err(error) = channel.read_message().await {
            let _ = ready_sender.send(Err(map_device_error(error)));
            return;
        }
        if ready_sender.send(Ok(())).is_err() {
            let _ = channel
                .call_method(Some(plist::Value::String("stop".into())), None, false)
                .await;
            return;
        }
        loop {
            tokio::select! {
                () = worker_cancel.cancelled() => {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        channel.call_method(
                            Some(plist::Value::String("stop".into())),
                            None,
                            false,
                        ),
                    ).await;
                    return;
                }
                result = next_sysmontap_json_line(&mut channel) => {
                    let failed = result.is_err();
                    if sender.send(result).await.is_err() || failed {
                        return;
                    }
                }
            }
        }
    });
    match ready_receiver.await {
        Ok(Ok(())) => Ok(Box::new(IdeviceDvtWorkerStream {
            receiver,
            cancel,
            join: Some(join),
            info: DeviceStreamInfo {
                content_type: "application/x-ndjson".into(),
                initial_bytes: Vec::new(),
                service_transport,
            },
        })),
        Ok(Err(error)) => {
            let _ = join.await;
            Err(error)
        }
        Err(_) => {
            let _ = join.await;
            Err(CoreError::Backend(
                "sysmontap worker exited during setup".into(),
            ))
        }
    }
}

fn notification_json_line(
    notification: idevice::dvt::notifications::NotificationInfo,
) -> CoreResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&json!({
        "notificationType": notification.notification_type,
        "machAbsoluteTime": notification.mach_absolute_time,
        "execName": notification.exec_name,
        "appName": notification.app_name,
        "pid": notification.pid,
        "stateDescription": notification.state_description,
    }))
    .map_err(|error| CoreError::Backend(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

async fn sysmontap_attributes(remote: &mut DvtClient) -> CoreResult<(Vec<String>, Vec<String>)> {
    let mut device_info = idevice::dvt::device_info::DeviceInfoClient::new(remote)
        .await
        .map_err(map_device_error)?;
    let _ = device_info
        .sysmon_process_attributes()
        .await
        .map_err(map_device_error)?;
    let system_attributes = device_info
        .sysmon_system_attributes()
        .await
        .map_err(map_device_error)?;
    let process_attributes = [
        "pid",
        "name",
        "cpuUsage",
        "memAnon",
        "physFootprint",
        "diskBytesRead",
        "diskBytesWritten",
        "threadCount",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    Ok((process_attributes, system_attributes))
}

fn sysmontap_config(
    process_attributes: Vec<String>,
    system_attributes: Vec<String>,
) -> plist::Dictionary {
    let mut config = plist::Dictionary::new();
    config.insert("ur".into(), plist::Value::Integer(10_i64.into()));
    config.insert("bm".into(), plist::Value::Integer(0_i64.into()));
    config.insert(
        "procAttrs".into(),
        plist::Value::Array(
            process_attributes
                .into_iter()
                .map(plist::Value::String)
                .collect(),
        ),
    );
    config.insert(
        "sysAttrs".into(),
        plist::Value::Array(
            system_attributes
                .into_iter()
                .map(plist::Value::String)
                .collect(),
        ),
    );
    config.insert("cpuUsage".into(), plist::Value::Boolean(true));
    config.insert("physFootprint".into(), plist::Value::Boolean(true));
    config.insert(
        "sampleInterval".into(),
        plist::Value::Integer(500_000_000_i64.into()),
    );
    config
}

async fn next_sysmontap_json_line<R: idevice::ReadWrite>(
    channel: &mut idevice::dvt::remote_server::Channel<'_, R>,
) -> CoreResult<Vec<u8>> {
    let sample = loop {
        let message = channel.read_message().await.map_err(map_device_error)?;
        if let Some(data) = message.data
            && let Some(sample) = find_sysmontap_sample(data)
        {
            break sample;
        }
    };
    let processes = sample
        .get("Processes")
        .cloned()
        .map(plist_json)
        .transpose()?;
    let system = sample.get("System").cloned().map(plist_json).transpose()?;
    let system_cpu_usage = sample
        .get("SystemCPUUsage")
        .cloned()
        .map(plist_json)
        .transpose()?;
    let mut bytes = serde_json::to_vec(&json!({
        "processes": processes,
        "system": system,
        "systemCpuUsage": system_cpu_usage,
    }))
    .map_err(|error| CoreError::Backend(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn find_sysmontap_sample(value: plist::Value) -> Option<plist::Dictionary> {
    match value {
        plist::Value::Dictionary(dictionary)
            if dictionary.contains_key("Processes")
                || dictionary.contains_key("System")
                || dictionary.contains_key("SystemCPUUsage") =>
        {
            Some(dictionary)
        }
        plist::Value::Dictionary(dictionary) => dictionary
            .into_iter()
            .map(|(_, value)| value)
            .find_map(find_sysmontap_sample),
        plist::Value::Array(values) => values.into_iter().find_map(find_sysmontap_sample),
        _ => None,
    }
}

#[async_trait]
impl DeviceStream for IdeviceOsTraceStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        let entry = loop {
            let entry = self.receiver.next().await.map_err(map_device_error)?;
            if os_trace_entry_matches(&entry, &self.options) {
                break entry;
            }
        };
        if self.format == StreamFormat::JsonLines {
            let mut bytes = serde_json::to_vec(&entry)
                .map_err(|error| CoreError::Backend(error.to_string()))?;
            bytes.push(b'\n');
            Ok(bytes)
        } else {
            let label = entry.label.map_or_else(String::new, |label| {
                format!(" [{}:{}]", label.subsystem, label.category)
            });
            Ok(format!(
                "{} {} {:?} {}{}: {}\n",
                entry.timestamp, entry.pid, entry.level, entry.image_name, label, entry.message
            )
            .into_bytes())
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

fn os_trace_entry_matches(
    entry: &idevice::os_trace_relay::OsTraceLog,
    options: &OsTraceOptions,
) -> bool {
    if !options.levels.is_empty()
        && !options
            .levels
            .iter()
            .any(|level| os_trace_level_matches(entry.level, *level))
    {
        return false;
    }
    if options.subsystem_contains.as_ref().is_some_and(|filter| {
        entry
            .label
            .as_ref()
            .is_none_or(|label| !label.subsystem.contains(filter))
    }) {
        return false;
    }
    if options
        .message_contains
        .as_ref()
        .is_some_and(|filter| !entry.message.contains(filter))
    {
        return false;
    }
    if options
        .message_excludes
        .as_ref()
        .is_some_and(|filter| entry.message.contains(filter))
    {
        return false;
    }
    true
}

fn os_trace_level_matches(
    level: idevice::os_trace_relay::LogLevel,
    requested: OsTraceLevel,
) -> bool {
    matches!(
        (level, requested),
        (
            idevice::os_trace_relay::LogLevel::Notice,
            OsTraceLevel::Default
        ) | (idevice::os_trace_relay::LogLevel::Info, OsTraceLevel::Info)
            | (
                idevice::os_trace_relay::LogLevel::Debug,
                OsTraceLevel::Debug
            )
            | (
                idevice::os_trace_relay::LogLevel::Error,
                OsTraceLevel::Error
            )
            | (
                idevice::os_trace_relay::LogLevel::Fault,
                OsTraceLevel::Fault
            )
    )
}

#[async_trait]
impl DeviceStream for IdevicePcapStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        loop {
            let packet = self.client.next_packet().await.map_err(map_device_error)?;
            if self
                .pid
                .is_some_and(|pid| packet.pid != pid && packet.epid != pid)
            {
                continue;
            }
            if self.process.as_ref().is_some_and(|process| {
                !packet.comm.starts_with(process) && !packet.ecomm.starts_with(process)
            }) {
                continue;
            }
            return Ok(pcap_record(&packet));
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

fn pcap_header() -> Vec<u8> {
    vec![
        0xd4, 0xc3, 0xb2, 0xa1, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xff, 0xff, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ]
}

fn pcap_record(packet: &idevice::pcapd::DevicePacket) -> Vec<u8> {
    let length = packet.data.len() as u32;
    let mut result = Vec::with_capacity(16 + packet.data.len());
    result.extend_from_slice(&packet.seconds.to_le_bytes());
    result.extend_from_slice(&packet.microseconds.to_le_bytes());
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(&packet.data);
    result
}

#[cfg(windows)]
async fn start_windows_kernel_tunnel(
    backend: &IdeviceBackend,
    spec: &TunnelSpec,
) -> CoreResult<KernelTunnelHandle> {
    if spec.transport != TunnelTransport::UsbCoreDevice {
        return Err(CoreError::InvalidArgument(
            "kernel tunnel only supports UsbCoreDevice transport; RemotePairingTcp is userspace-only"
                .into(),
        ));
    }
    let wintun_path = resolve_wintun_path(spec)?;
    ensure_windows_kernel_permissions()?;

    let target = DeviceTarget {
        udid: spec.udid.clone(),
        ..DeviceTarget::default()
    };
    let raw_device = backend.raw_device(&target).await?;
    let udid = raw_device.udid.clone();
    let provider = raw_device.to_provider(backend.addr.clone(), "rust-ios-kernel-tunnel");
    let proxy = CoreDeviceProxy::connect(&provider)
        .await
        .map_err(map_device_error)?;
    let tunnel_info = proxy.tunnel_info().clone();
    let client_address = tunnel_info
        .client_address
        .parse::<Ipv6Addr>()
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "CDTunnel returned invalid client IPv6 address {:?}: {error}",
                tunnel_info.client_address
            ))
        })?;
    let interface_name = spec
        .kernel_interface_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_kernel_interface_name(&udid));

    let device = tun_rs::DeviceBuilder::new()
        .name(interface_name)
        .description("rust-ios CoreDevice tunnel")
        .mtu(tunnel_info.mtu)
        .ipv6(client_address, 64)
        .wintun_file(wintun_path.to_string_lossy().into_owned())
        .build_async()
        .map_err(|error| map_windows_tun_error("create and configure Wintun adapter", error))?;
    let interface_name = device
        .name()
        .map_err(|error| map_windows_tun_error("read Wintun adapter name", error))?;
    let transport = proxy.into_tunnel().into_inner();
    let (reader, writer) = tokio::io::split(transport);
    let netmask = if tunnel_info.netmask.trim().is_empty() {
        "ffff:ffff:ffff:ffff::".to_owned()
    } else {
        tunnel_info.netmask
    };
    let route_info = UserSpaceTunnelInfo {
        udid: udid.clone(),
        client_address: tunnel_info.client_address,
        server_address: tunnel_info.server_address,
        netmask: netmask.clone(),
        mtu: tunnel_info.mtu,
        rsd_port: tunnel_info.server_rsd_port,
    };
    let kernel_info = KernelTunnelInfo {
        udid,
        interface_name,
        client_address: route_info.client_address.clone(),
        server_address: route_info.server_address.clone(),
        netmask,
        mtu: route_info.mtu,
        rsd_port: route_info.rsd_port,
    };
    let route: Arc<dyn UserSpaceTunnel> = Arc::new(IdeviceKernelRouteTunnel {
        info: route_info,
        rsd: Mutex::new(None),
        closed: AtomicBool::new(false),
    });
    Ok(KernelTunnelHandle {
        info: kernel_info,
        route,
        runner: Box::new(IdeviceKernelTunnelRunner {
            device: Some(Arc::new(device)),
            reader: Some(reader),
            writer: Some(writer),
            mtu: usize::from(tunnel_info.mtu),
            closed: false,
        }),
    })
}

#[cfg(windows)]
fn resolve_wintun_path(spec: &TunnelSpec) -> CoreResult<PathBuf> {
    let path = spec
        .wintun_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\wintun.dll"));
    if !path.is_absolute() {
        return Err(CoreError::InvalidArgument(
            "Wintun DLL path must be absolute; CLI clients resolve relative paths".into(),
        ));
    }
    if !path.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "Wintun DLL does not exist or is not a file: {}",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(windows)]
fn ensure_windows_kernel_permissions() -> CoreResult<()> {
    std::fs::File::open(r"\\.\PHYSICALDRIVE0")
        .map(drop)
        .map_err(|error| {
            CoreError::PermissionDenied(format!(
                "Windows kernel tunnel requires an elevated administrator process: {error}"
            ))
        })
}

#[cfg(windows)]
fn default_kernel_interface_name(udid: &str) -> String {
    let suffix: String = udid
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(8)
        .collect();
    if suffix.is_empty() {
        "rust-ios".into()
    } else {
        format!("rust-ios-{suffix}")
    }
}

#[cfg(windows)]
fn map_windows_tun_error(context: &str, error: std::io::Error) -> CoreError {
    if error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(5) {
        CoreError::PermissionDenied(format!("{context}: {error}"))
    } else {
        CoreError::Backend(format!("{context}: {error}"))
    }
}

#[cfg(windows)]
#[async_trait]
impl KernelTunnelRunner for IdeviceKernelTunnelRunner {
    async fn run(&mut self, cancel: CancellationToken) -> CoreResult<()> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "kernel tunnel runner is closed".into(),
            ));
        }
        let device = self
            .device
            .as_ref()
            .cloned()
            .ok_or_else(|| CoreError::DeviceUnavailable("Wintun adapter is closed".into()))?;
        let reader = self.reader.take().ok_or_else(|| {
            CoreError::DeviceUnavailable("kernel tunnel reader is unavailable".into())
        })?;
        let writer = self.writer.take().ok_or_else(|| {
            CoreError::DeviceUnavailable("kernel tunnel writer is unavailable".into())
        })?;
        let mut device_to_phone =
            Box::pin(pump_wintun_to_coredevice(device.clone(), writer, self.mtu));
        let mut phone_to_device = Box::pin(pump_coredevice_to_wintun(device, reader, self.mtu));
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(()),
            result = &mut device_to_phone => result,
            result = &mut phone_to_device => result,
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.reader.take();
            self.writer.take();
            self.device.take();
        }
        Ok(())
    }
}

#[cfg(windows)]
async fn pump_wintun_to_coredevice(
    device: Arc<tun_rs::AsyncDevice>,
    mut writer: tokio::io::WriteHalf<Box<dyn idevice::ReadWrite>>,
    mtu: usize,
) -> CoreResult<()> {
    let mut packet = vec![0_u8; mtu.max(1280)];
    loop {
        let length = device.recv(&mut packet).await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("read packet from Wintun adapter: {error}"))
        })?;
        if length > 0 && packet[0] >> 4 != 6 {
            continue;
        }
        validate_ipv6_packet(&packet[..length], mtu)?;
        writer.write_all(&packet[..length]).await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("write packet to CoreDevice tunnel: {error}"))
        })?;
        writer.flush().await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("flush CoreDevice tunnel packet: {error}"))
        })?;
    }
}

#[cfg(windows)]
async fn pump_coredevice_to_wintun(
    device: Arc<tun_rs::AsyncDevice>,
    mut reader: tokio::io::ReadHalf<Box<dyn idevice::ReadWrite>>,
    mtu: usize,
) -> CoreResult<()> {
    let mut header = [0_u8; 40];
    loop {
        reader.read_exact(&mut header).await.map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "read IPv6 header from CoreDevice tunnel: {error}"
            ))
        })?;
        let payload_length = usize::from(u16::from_be_bytes([header[4], header[5]]));
        let packet_length = header.len() + payload_length;
        if packet_length > mtu {
            return Err(CoreError::DeviceUnavailable(format!(
                "CoreDevice IPv6 packet length {packet_length} exceeds negotiated MTU {mtu}"
            )));
        }
        let mut packet = vec![0_u8; packet_length];
        packet[..header.len()].copy_from_slice(&header);
        reader
            .read_exact(&mut packet[header.len()..])
            .await
            .map_err(|error| {
                CoreError::DeviceUnavailable(format!(
                    "read IPv6 payload from CoreDevice tunnel: {error}"
                ))
            })?;
        validate_ipv6_packet(&packet, mtu)?;
        let written = device.send(&packet).await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("write packet to Wintun adapter: {error}"))
        })?;
        if written != packet.len() {
            return Err(CoreError::DeviceUnavailable(format!(
                "Wintun adapter accepted {written} of {} packet bytes",
                packet.len()
            )));
        }
    }
}

#[cfg(windows)]
fn validate_ipv6_packet(packet: &[u8], mtu: usize) -> CoreResult<()> {
    if packet.len() < 40 {
        return Err(CoreError::DeviceUnavailable(format!(
            "kernel tunnel received a truncated IPv6 packet ({} bytes)",
            packet.len()
        )));
    }
    if packet[0] >> 4 != 6 {
        return Err(CoreError::DeviceUnavailable(format!(
            "kernel tunnel received IP version {} instead of IPv6",
            packet[0] >> 4
        )));
    }
    if packet.len() > mtu {
        return Err(CoreError::DeviceUnavailable(format!(
            "kernel tunnel received {} bytes above negotiated MTU {mtu}",
            packet.len()
        )));
    }
    let expected = 40 + usize::from(u16::from_be_bytes([packet[4], packet[5]]));
    if packet.len() != expected {
        return Err(CoreError::DeviceUnavailable(format!(
            "IPv6 header declares {expected} bytes but packet contains {}",
            packet.len()
        )));
    }
    Ok(())
}

#[cfg(windows)]
#[async_trait]
impl UserSpaceTunnel for IdeviceKernelRouteTunnel {
    fn info(&self) -> &UserSpaceTunnelInfo {
        &self.info
    }

    async fn connect(&self, port: u16) -> CoreResult<BoxedIo> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "kernel tunnel route is closed".into(),
            ));
        }
        let address = self
            .info
            .server_address
            .parse::<Ipv6Addr>()
            .map_err(|error| {
                CoreError::DeviceUnavailable(format!(
                    "invalid kernel tunnel server address {:?}: {error}",
                    self.info.server_address
                ))
            })?;
        let stream = TcpStream::connect(SocketAddrV6::new(address, port, 0, 0))
            .await
            .map_err(|error| {
                CoreError::DeviceUnavailable(format!(
                    "connect [{}]:{port} through kernel tunnel: {error}",
                    self.info.server_address
                ))
            })?;
        Ok(Box::new(stream))
    }

    async fn rsd_services(&self) -> CoreResult<BTreeMap<String, RsdServiceInfo>> {
        let handshake = self.rsd_handshake().await?;
        Ok(handshake
            .services
            .into_iter()
            .map(|(name, service)| {
                (
                    name,
                    RsdServiceInfo {
                        port: u32::from(service.port),
                    },
                )
            })
            .collect())
    }

    async fn connect_rsd_service(&self, service: RsdServiceKind) -> CoreResult<BoxedIo> {
        let name = service.service_name();
        let handshake = self.rsd_handshake().await?;
        let port = handshake
            .services
            .get(name)
            .map(|service| service.port)
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!("RSD service {name:?} is not advertised"))
            })?;
        match self.connect(port).await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                self.rsd.lock().await.take();
                Err(error)
            }
        }
    }

    async fn close(&self) -> CoreResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.rsd.lock().await.take();
        Ok(())
    }
}

#[cfg(windows)]
impl IdeviceKernelRouteTunnel {
    async fn rsd_handshake(&self) -> CoreResult<idevice::rsd::RsdHandshake> {
        const ATTEMPTS: usize = 3;
        const TIMEOUT: Duration = Duration::from_secs(5);

        let mut cached = self.rsd.lock().await;
        if cached.is_none() {
            let mut last_error = None;
            for attempt in 1..=ATTEMPTS {
                let stream = match self.connect(self.info.rsd_port).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        last_error = Some(error);
                        continue;
                    }
                };
                match tokio::time::timeout(TIMEOUT, idevice::rsd::RsdHandshake::new(stream)).await {
                    Ok(Ok(handshake)) => {
                        *cached = Some(handshake);
                        break;
                    }
                    Ok(Err(error)) => last_error = Some(map_device_error(error)),
                    Err(_) => {
                        last_error = Some(CoreError::DeviceUnavailable(format!(
                            "kernel RSD handshake attempt {attempt}/{ATTEMPTS} timed out after {} seconds",
                            TIMEOUT.as_secs()
                        )));
                    }
                }
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            if cached.is_none() {
                return Err(last_error.unwrap_or_else(|| {
                    CoreError::DeviceUnavailable(format!(
                        "kernel RSD handshake failed after {ATTEMPTS} attempts"
                    ))
                }));
            }
        }
        cached
            .clone()
            .ok_or_else(|| CoreError::Backend("kernel RSD cache was not initialized".into()))
    }
}

#[async_trait]
impl UserSpaceTunnel for IdeviceUserSpaceTunnel {
    fn info(&self) -> &UserSpaceTunnelInfo {
        &self.info
    }

    async fn connect(&self, port: u16) -> CoreResult<BoxedIo> {
        let mut guard = self.adapter.lock().await;
        let adapter = guard
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("tunnel is closed".into()))?;
        let stream = adapter.connect(port).await.map_err(CoreError::Io)?;
        Ok(Box::new(stream))
    }

    async fn rsd_services(&self) -> CoreResult<BTreeMap<String, RsdServiceInfo>> {
        let handshake = self.rsd_handshake().await?;
        Ok(handshake
            .services
            .into_iter()
            .map(|(name, service)| {
                (
                    name,
                    RsdServiceInfo {
                        port: u32::from(service.port),
                    },
                )
            })
            .collect())
    }

    async fn connect_rsd_service(&self, service: RsdServiceKind) -> CoreResult<BoxedIo> {
        let name = service.service_name();
        let handshake = self.rsd_handshake().await?;
        let port = handshake
            .services
            .get(name)
            .map(|service| service.port)
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!("RSD service {name:?} is not advertised"))
            })?;
        match self.connect(port).await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                self.rsd.lock().await.take();
                Err(error)
            }
        }
    }

    async fn close(&self) -> CoreResult<()> {
        self.rsd.lock().await.take();
        if let Some(mut adapter) = self.adapter.lock().await.take() {
            adapter.close().await.map_err(CoreError::Io)?;
        }
        Ok(())
    }
}

impl IdeviceUserSpaceTunnel {
    async fn rsd_handshake(&self) -> CoreResult<idevice::rsd::RsdHandshake> {
        const ATTEMPTS: usize = 3;
        const TIMEOUT: Duration = Duration::from_secs(5);

        let mut cached = self.rsd.lock().await;
        if cached.is_none() {
            let mut last_error = None;
            for attempt in 1..=ATTEMPTS {
                let stream = match self.connect(self.info.rsd_port).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        last_error = Some(error);
                        continue;
                    }
                };
                match tokio::time::timeout(TIMEOUT, idevice::rsd::RsdHandshake::new(stream)).await {
                    Ok(Ok(handshake)) => {
                        *cached = Some(handshake);
                        break;
                    }
                    Ok(Err(error)) => last_error = Some(map_device_error(error)),
                    Err(_) => {
                        last_error = Some(CoreError::DeviceUnavailable(format!(
                            "RSD handshake attempt {attempt}/{ATTEMPTS} timed out after {} seconds",
                            TIMEOUT.as_secs()
                        )));
                    }
                }
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            if cached.is_none() {
                return Err(last_error.unwrap_or_else(|| {
                    CoreError::DeviceUnavailable(format!(
                        "RSD handshake failed after {ATTEMPTS} attempts"
                    ))
                }));
            }
        }
        cached
            .clone()
            .ok_or_else(|| CoreError::Backend("RSD handshake cache was not initialized".into()))
    }
}

type DvtClient = idevice::dvt::remote_server::RemoteServerClient<Box<dyn idevice::ReadWrite>>;

async fn connect_dvt(
    provider: &dyn IdeviceProvider,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<DvtClient> {
    if let Some(tunnel) = tunnel {
        let stream = tunnel.connect_rsd_service(RsdServiceKind::Dvt).await?;
        return Ok(DvtClient::new(stream));
    }

    let mut lockdown = lockdown_with_session(provider).await?;
    let version = lockdown_string_value(&mut lockdown, "ProductVersion", None).await?;
    if ios_major_version(&version)? >= 17 {
        return Err(CoreError::DeviceUnavailable(format!(
            "iOS {version} DVT requires a running server-owned userspace tunnel; run tunnel start --userspace"
        )));
    }
    DvtClient::connect(provider).await.map_err(map_device_error)
}

async fn resolve_process_pid(
    provider: &dyn IdeviceProvider,
    process_name: &str,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<u32> {
    let mut remote = connect_dvt(provider, tunnel).await?;
    let mut service = idevice::dvt::device_info::DeviceInfoClient::new(&mut remote)
        .await
        .map_err(map_device_error)?;
    service
        .running_processes()
        .await
        .map_err(map_device_error)?
        .into_iter()
        .find(|process| process.name == process_name || process.real_app_name == process_name)
        .map(|process| process.pid)
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "process {process_name:?} is not currently running"
            ))
        })
}

fn dvt_service_transport(tunnel: Option<&Arc<dyn UserSpaceTunnel>>) -> ServiceTransport {
    if tunnel.is_some() {
        ServiceTransport::SharedRsd
    } else {
        ServiceTransport::Lockdown
    }
}

fn command_uses_shared_rsd(invocation: &CommandInvocation) -> bool {
    match invocation.argv.first().map(String::as_str) {
        Some("file") => crate::file::has_domain_selector(&invocation.argv),
        Some("info") => invocation.argv.get(1).map(String::as_str) == Some("display"),
        Some(
            "ps" | "launch" | "kill" | "memlimitoff" | "devicestate" | "webinspector"
            | "screenshot" | "setlocation" | "resetlocation",
        ) => true,
        _ => false,
    }
}

async fn command_display_info(tunnel: Option<&dyn UserSpaceTunnel>) -> CoreResult<Value> {
    const TIMEOUT: Duration = Duration::from_secs(10);

    let tunnel = tunnel.ok_or_else(|| {
        CoreError::DeviceUnavailable(
            "info display requires a running server-owned userspace tunnel; run tunnel start --userspace"
                .into(),
        )
    })?;
    tunnel
        .ensure_rsd_service(RsdServiceKind::DeviceInfo)
        .await?;
    let stream = tokio::time::timeout(
        TIMEOUT,
        tunnel.connect_rsd_service(RsdServiceKind::DeviceInfo),
    )
    .await
    .map_err(|_| {
        CoreError::Timeout("info display timed out connecting deviceinfo service".into())
    })??;
    let mut client = tokio::time::timeout(
        TIMEOUT,
        ios_device_core::deviceinfo::DeviceInfoClient::connect_stream(
            stream,
            uuid::Uuid::new_v4().to_string(),
        ),
    )
    .await
    .map_err(|_| CoreError::Timeout("info display timed out during XPC handshake".into()))?
    .map_err(|error| {
        CoreError::DeviceUnavailable(format!("info display XPC handshake failed: {error}"))
    })?;
    let output = tokio::time::timeout(TIMEOUT, client.get_display_info())
        .await
        .map_err(|_| CoreError::Timeout("info display request timed out".into()))?
        .map_err(|error| CoreError::Backend(format!("info display request failed: {error}")))?;
    plist_json(ios_device_core::deviceinfo::xpc_value_to_plist(&output))
}

async fn command_ip(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let timeout_seconds = option_value(&invocation.argv, "--timeout")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                CoreError::InvalidArgument(format!(
                    "ip --timeout must be an integer number of seconds, got {value:?}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(60);
    if !(1..=3600).contains(&timeout_seconds) {
        return Err(CoreError::InvalidArgument(
            "ip --timeout must be between 1 and 3600 seconds".into(),
        ));
    }

    let mut lockdown = lockdown_with_session(provider).await?;
    let mac_text = lockdown_string_value(&mut lockdown, "WiFiAddress", None).await?;
    let mac = parse_mac_address(&mac_text)?;
    drop(lockdown);

    let mut pcap = idevice::pcapd::PcapdClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let discover = async {
        let mut ipv4 = None;
        let mut ipv6 = None;
        loop {
            let packet = pcap.next_packet().await.map_err(map_device_error)?;
            match packet_source_ip(&packet.data, mac) {
                Some(std::net::IpAddr::V4(address)) => ipv4 = Some(address),
                Some(std::net::IpAddr::V6(address)) => ipv6 = Some(address),
                None => {}
            }
            if let (Some(ipv4), Some(ipv6)) = (ipv4, ipv6) {
                return Ok::<_, CoreError>((ipv4, ipv6));
            }
        }
    };
    let (ipv4, ipv6) = tokio::time::timeout(Duration::from_secs(timeout_seconds), discover)
        .await
        .map_err(|_| {
            CoreError::Timeout(format!(
                "ip did not observe both IPv4 and IPv6 traffic from {mac_text} within {timeout_seconds} seconds; generate network traffic and check Private Wi-Fi Address"
            ))
        })??;
    Ok(json!({
        "Mac": mac_text,
        "IPv4": ipv4.to_string(),
        "IPv6": ipv6.to_string(),
    }))
}

async fn command_device_state_list(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let action =
        invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
            CoreError::InvalidArgument("devicestate requires list or enable".into())
        })?;
    if action != "list" {
        return Err(CoreError::NotImplemented(
            "devicestate enable must be started through the server task manager".into(),
        ));
    }
    let mut remote = connect_dvt(provider, tunnel).await?;
    let mut control = idevice::dvt::condition_inducer::ConditionInducerClient::new(&mut remote)
        .await
        .map_err(map_device_error)?;
    let groups = control
        .available_conditions()
        .await
        .map_err(map_device_error)?;
    Ok(Value::Array(
        groups
            .into_iter()
            .map(|group| {
                json!({
                    "Identifier": group.identifier,
                    "Profiles": group.profiles.into_iter().map(|profile| json!({
                        "Identifier": profile.identifier,
                        "Description": profile.description,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    ))
}

fn parse_mac_address(value: &str) -> CoreResult<[u8; 6]> {
    let parts: Vec<_> = value.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(CoreError::Backend(format!(
            "Lockdown WiFiAddress {value:?} is not a six-byte MAC address"
        )));
    }
    let mut result = [0_u8; 6];
    for (target, part) in result.iter_mut().zip(parts) {
        if part.len() != 2 {
            return Err(CoreError::Backend(format!(
                "Lockdown WiFiAddress {value:?} has an invalid component"
            )));
        }
        *target = u8::from_str_radix(part, 16).map_err(|_| {
            CoreError::Backend(format!(
                "Lockdown WiFiAddress {value:?} contains non-hex digits"
            ))
        })?;
    }
    Ok(result)
}

fn packet_source_ip(frame: &[u8], source_mac: [u8; 6]) -> Option<std::net::IpAddr> {
    if frame.len() < 14 || frame[6..12] != source_mac {
        return None;
    }
    let mut ether_type = u16::from_be_bytes(frame[12..14].try_into().ok()?);
    let mut offset = 14;
    while matches!(ether_type, 0x8100 | 0x88a8) {
        let header = frame.get(offset..offset + 4)?;
        ether_type = u16::from_be_bytes(header[2..4].try_into().ok()?);
        offset += 4;
    }
    match ether_type {
        0x0800 => {
            let packet = frame.get(offset..offset + 20)?;
            if packet[0] >> 4 != 4 || packet[0] & 0x0f < 5 {
                return None;
            }
            Some(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                packet[12], packet[13], packet[14], packet[15],
            )))
        }
        0x86dd => {
            let packet = frame.get(offset..offset + 40)?;
            if packet[0] >> 4 != 6 {
                return None;
            }
            let source: [u8; 16] = packet[8..24].try_into().ok()?;
            Some(std::net::IpAddr::V6(std::net::Ipv6Addr::from(source)))
        }
        _ => None,
    }
}

#[derive(Debug)]
struct LanguageConfiguration {
    language: String,
    locale: String,
    supported_locales: Vec<String>,
    supported_languages: Vec<String>,
}

impl LanguageConfiguration {
    fn into_json(self) -> Value {
        json!({
            "Language": self.language,
            "Locale": self.locale,
            "SupportedLocales": self.supported_locales,
            "SupportedLanguages": self.supported_languages,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleAction {
    Get,
    Set(bool),
    Toggle,
}

#[derive(Debug, Clone, Copy)]
struct LockdownToggleSpec {
    key: &'static str,
    domain: Option<&'static str>,
    output_key: &'static str,
    action: ToggleAction,
    time_format: bool,
}

async fn lockdown_with_session(
    provider: &dyn IdeviceProvider,
) -> CoreResult<idevice::lockdown::LockdownClient> {
    let mut client = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(map_device_error)?;
    client
        .start_session(&pairing)
        .await
        .map_err(map_device_error)?;
    Ok(client)
}

async fn read_language_configuration(
    client: &mut idevice::lockdown::LockdownClient,
) -> CoreResult<LanguageConfiguration> {
    const DOMAIN: &str = "com.apple.international";
    let language = lockdown_string_value(client, "Language", Some(DOMAIN)).await?;
    let locale = lockdown_string_value(client, "Locale", Some(DOMAIN)).await?;
    let supported_locales = lockdown_string_list(client, "SupportedLocales", Some(DOMAIN)).await?;
    let supported_languages =
        lockdown_string_list(client, "SupportedLanguages", Some(DOMAIN)).await?;
    Ok(LanguageConfiguration {
        language,
        locale,
        supported_locales,
        supported_languages,
    })
}

async fn lockdown_string_value(
    client: &mut idevice::lockdown::LockdownClient,
    key: &str,
    domain: Option<&str>,
) -> CoreResult<String> {
    let value = client
        .get_value(Some(key), domain)
        .await
        .map_err(map_device_error)?;
    value.as_string().map(str::to_owned).ok_or_else(|| {
        CoreError::Backend(format!(
            "Lockdown {key} returned {}, expected a string",
            plist_value_kind(&value)
        ))
    })
}

async fn lockdown_string_list(
    client: &mut idevice::lockdown::LockdownClient,
    key: &str,
    domain: Option<&str>,
) -> CoreResult<Vec<String>> {
    let value = client
        .get_value(Some(key), domain)
        .await
        .map_err(map_device_error)?;
    let values = value.as_array().ok_or_else(|| {
        CoreError::Backend(format!(
            "Lockdown {key} returned {}, expected an array",
            plist_value_kind(&value)
        ))
    })?;
    values
        .iter()
        .map(|value| {
            value.as_string().map(str::to_owned).ok_or_else(|| {
                CoreError::Backend(format!(
                    "Lockdown {key} contains {}, expected only strings",
                    plist_value_kind(value)
                ))
            })
        })
        .collect()
}

async fn command_language(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    const DOMAIN: &str = "com.apple.international";
    let requested_locale = optional_nonempty_value(&invocation.argv, "--setlocale")?;
    let requested_language = optional_nonempty_value(&invocation.argv, "--setlang")?;
    let mut client = lockdown_with_session(provider).await?;
    let before = read_language_configuration(&mut client).await?;

    let locale_changed = requested_locale
        .as_ref()
        .is_some_and(|locale| *locale != before.locale);
    let language_changed = requested_language
        .as_ref()
        .is_some_and(|language| *language != before.language);
    if !locale_changed && !language_changed {
        return Ok(before.into_json());
    }
    validate_language_request(
        &before,
        requested_locale.as_deref().filter(|_| locale_changed),
        requested_language.as_deref().filter(|_| language_changed),
    )?;

    if let Some(locale) = requested_locale.as_ref().filter(|_| locale_changed) {
        client
            .set_value("Locale", plist::Value::String(locale.clone()), Some(DOMAIN))
            .await
            .map_err(map_device_error)?;
    }
    if let Some(language) = requested_language.as_ref().filter(|_| language_changed)
        && let Err(error) = client
            .set_value(
                "Language",
                plist::Value::String(language.clone()),
                Some(DOMAIN),
            )
            .await
    {
        let error = map_device_error(error);
        if locale_changed
            && let Err(rollback_error) = client
                .set_value(
                    "Locale",
                    plist::Value::String(before.locale.clone()),
                    Some(DOMAIN),
                )
                .await
                .map_err(map_device_error)
        {
            return Err(CoreError::Backend(format!(
                "language write failed: {error}; locale rollback failed: {rollback_error}"
            )));
        }
        return Err(error);
    }
    drop(client);

    // Language changes restart SpringBoard and temporarily drop Lockdown. Polling also verifies
    // that SetValue was accepted instead of trusting an empty response from the service.
    let timeout = if language_changed {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(15)
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match lockdown_with_session(provider).await {
            Ok(mut client) => match read_language_configuration(&mut client).await {
                Ok(configuration)
                    if requested_locale
                        .as_ref()
                        .is_none_or(|locale| configuration.locale == *locale)
                        && requested_language
                            .as_ref()
                            .is_none_or(|language| configuration.language == *language) =>
                {
                    return Ok(configuration.into_json());
                }
                Ok(_) => {}
                Err(error) if !error.retryable() => return Err(error),
                Err(_) => {}
            },
            Err(error) if !error.retryable() => return Err(error),
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CoreError::Timeout(format!(
                "Lockdown did not report requested language {:?} and locale {:?} within {} seconds",
                requested_language,
                requested_locale,
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn validate_language_request(
    current: &LanguageConfiguration,
    requested_locale: Option<&str>,
    requested_language: Option<&str>,
) -> CoreResult<()> {
    if let Some(locale) = requested_locale
        && !current
            .supported_locales
            .iter()
            .any(|supported| supported == locale)
    {
        return Err(CoreError::InvalidArgument(format!(
            "lang --setlocale {locale:?} is not present in SupportedLocales"
        )));
    }
    if let Some(language) = requested_language
        && !current
            .supported_languages
            .iter()
            .any(|supported| supported == language)
    {
        return Err(CoreError::InvalidArgument(format!(
            "lang --setlang {language:?} is not present in SupportedLanguages"
        )));
    }
    Ok(())
}

async fn command_lockdown_toggle(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let command = invocation.argv[0].as_str();
    let action =
        invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
            CoreError::InvalidArgument(format!("{command} requires an operation"))
        })?;
    let spec = lockdown_toggle_spec(command, action)?;
    let force = has_flag(&invocation.argv, "--force");
    let mut client = lockdown_with_session(provider).await?;

    if !force {
        let version = lockdown_string_value(&mut client, "ProductVersion", None).await?;
        let major = ios_major_version(&version)?;
        if major < 11 {
            return Err(CoreError::InvalidArgument(format!(
                "{command} requires iOS 11.0 or newer (device is {version}); use --force to override"
            )));
        }
    }

    let current = match client.get_value(Some(spec.key), spec.domain).await {
        Ok(value) => Some(lockdown_toggle_value(&value, spec.key)?),
        Err(_) if force && matches!(spec.action, ToggleAction::Set(_)) => None,
        Err(error) => return Err(map_device_error(error)),
    };
    let desired = match spec.action {
        ToggleAction::Get => current.expect("get requires a successfully read value"),
        ToggleAction::Set(value) => value,
        ToggleAction::Toggle => !current.expect("toggle requires a successfully read value"),
    };
    let should_write = spec.action != ToggleAction::Get && (force || current != Some(desired));
    if should_write {
        client
            .set_value(spec.key, plist::Value::Boolean(desired), spec.domain)
            .await
            .map_err(map_device_error)?;
        let verified = client
            .get_value(Some(spec.key), spec.domain)
            .await
            .map_err(map_device_error)?;
        let verified = lockdown_toggle_value(&verified, spec.key)?;
        if verified != desired {
            return Err(CoreError::Backend(format!(
                "Lockdown did not apply {}={desired}; read back {verified}",
                spec.key
            )));
        }
    }

    let mut result = serde_json::Map::new();
    if spec.time_format {
        result.insert(
            spec.output_key.into(),
            Value::String(if desired { "24h" } else { "12h" }.into()),
        );
    } else {
        result.insert(spec.output_key.into(), Value::Bool(desired));
    }
    if spec.action != ToggleAction::Get {
        result.insert("status".into(), Value::String("set".into()));
        result.insert("changed".into(), Value::Bool(should_write));
    }
    Ok(Value::Object(result))
}

fn lockdown_toggle_spec(command: &str, action: &str) -> CoreResult<LockdownToggleSpec> {
    let (key, domain, output_key, time_format) = match command {
        "assistivetouch" => (
            "AssistiveTouchEnabledByiTunes",
            Some("com.apple.Accessibility"),
            "AssistiveTouchEnabled",
            false,
        ),
        "voiceover" => (
            "VoiceOverTouchEnabledByiTunes",
            Some("com.apple.Accessibility"),
            "VoiceOverTouchEnabled",
            false,
        ),
        "zoom" => (
            "ZoomTouchEnabledByiTunes",
            Some("com.apple.Accessibility"),
            "ZoomTouchEnabled",
            false,
        ),
        "timeformat" => ("Uses24HourClock", None, "TimeFormat", true),
        _ => {
            return Err(CoreError::InvalidArgument(format!(
                "unknown Lockdown toggle command {command:?}"
            )));
        }
    };
    let action = match (time_format, action) {
        (_, "get") => ToggleAction::Get,
        (_, "toggle") => ToggleAction::Toggle,
        (false, "enable") | (true, "24h") => ToggleAction::Set(true),
        (false, "disable") | (true, "12h") => ToggleAction::Set(false),
        _ => {
            let expected = if time_format {
                "24h, 12h, toggle or get"
            } else {
                "enable, disable, toggle or get"
            };
            return Err(CoreError::InvalidArgument(format!(
                "{command} operation must be {expected}, got {action:?}"
            )));
        }
    };
    Ok(LockdownToggleSpec {
        key,
        domain,
        output_key,
        action,
        time_format,
    })
}

fn lockdown_toggle_value(value: &plist::Value, key: &str) -> CoreResult<bool> {
    if let Some(value) = value.as_boolean() {
        return Ok(value);
    }
    if let Some(value) = value.as_unsigned_integer() {
        return match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CoreError::Backend(format!(
                "Lockdown {key} returned integer {value}, expected 0 or 1"
            ))),
        };
    }
    if let Some(value) = value.as_signed_integer() {
        return match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CoreError::Backend(format!(
                "Lockdown {key} returned integer {value}, expected 0 or 1"
            ))),
        };
    }
    Err(CoreError::Backend(format!(
        "Lockdown {key} returned {}, expected bool or integer 0/1",
        plist_value_kind(value)
    )))
}

fn ios_major_version(version: &str) -> CoreResult<u32> {
    version
        .split('.')
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "Lockdown ProductVersion {version:?} has an invalid format"
            ))
        })
}

fn optional_nonempty_value(arguments: &[String], name: &str) -> CoreResult<Option<String>> {
    let prefix = format!("{name}=");
    for (index, value) in arguments.iter().enumerate() {
        if let Some(value) = value.strip_prefix(&prefix) {
            return Ok((!value.is_empty()).then(|| value.to_owned()));
        }
        if value == name {
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| CoreError::InvalidArgument(format!("{name} requires a value")))?;
            if value.starts_with('-') {
                return Err(CoreError::InvalidArgument(format!(
                    "{name} requires a value, got option {value:?}"
                )));
            }
            return Ok((!value.is_empty()).then(|| value.clone()));
        }
    }
    Ok(None)
}

async fn command_device_date(provider: &dyn IdeviceProvider) -> CoreResult<(Value, String)> {
    let mut client = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(map_device_error)?;
    client
        .start_session(&pairing)
        .await
        .map_err(map_device_error)?;
    let value = client
        .get_value(Some("TimeIntervalSince1970"), None)
        .await
        .map_err(map_device_error)?;
    device_date_response(value)
}

fn device_date_response(value: plist::Value) -> CoreResult<(Value, String)> {
    let seconds = match value {
        plist::Value::Real(value) => value,
        plist::Value::Integer(value) => value
            .as_signed()
            .map(|value| value as f64)
            .or_else(|| value.as_unsigned().map(|value| value as f64))
            .ok_or_else(|| {
                CoreError::Backend("TimeIntervalSince1970 integer is out of range".into())
            })?,
        other => {
            return Err(CoreError::Backend(format!(
                "TimeIntervalSince1970 must be numeric, got {}",
                plist_value_kind(&other)
            )));
        }
    };
    if !seconds.is_finite()
        || seconds.trunc() < i64::MIN as f64
        || seconds.trunc() > i64::MAX as f64
    {
        return Err(CoreError::Backend(format!(
            "TimeIntervalSince1970 is outside the supported range: {seconds}"
        )));
    }
    let formatted = chrono::Utc
        .timestamp_opt(seconds.trunc() as i64, 0)
        .single()
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "TimeIntervalSince1970 cannot be represented as UTC: {seconds}"
            ))
        })?
        .format("%A, %d-%b-%y %H:%M:%S UTC")
        .to_string();
    Ok((
        json!({
            "TimeIntervalSince1970": seconds,
            "formatedDate": formatted,
        }),
        formatted,
    ))
}

fn validate_diagnostics_list(arguments: &[String]) -> CoreResult<()> {
    if arguments.len() != 2 || arguments.get(1).map(String::as_str) != Some("list") {
        return Err(CoreError::InvalidArgument(
            "diagnostics requires exactly the list subcommand".into(),
        ));
    }
    Ok(())
}

fn diagnostics_list_response(value: Option<plist::Dictionary>) -> CoreResult<Value> {
    let diagnostics = value.ok_or_else(|| {
        CoreError::Backend(
            "diagnostics list response did not contain a Diagnostics dictionary".into(),
        )
    })?;
    let mut diagnostics = plist_json(diagnostics)?;
    let diagnostics = diagnostics.as_object_mut().ok_or_else(|| {
        CoreError::Backend("diagnostics list could not serialize Diagnostics as an object".into())
    })?;
    ensure_diagnostics_fields(
        diagnostics,
        "GasGauge",
        &[
            ("CycleCount", json!(0)),
            ("DesignCapacity", json!(0)),
            ("FullChargeCapacity", json!(0)),
            ("Status", json!("")),
        ],
    )?;
    ensure_diagnostics_fields(
        diagnostics,
        "HDMI",
        &[("Connection", json!("")), ("Status", json!(""))],
    )?;
    ensure_diagnostics_fields(diagnostics, "NAND", &[("Status", json!(""))])?;
    ensure_diagnostics_fields(
        diagnostics,
        "WiFi",
        &[("Active", json!("")), ("Status", json!(""))],
    )?;
    ensure_diagnostics_fields(
        diagnostics,
        "IORegistry",
        &[
            ("InstantAmperage", json!(0)),
            ("Temperature", json!(0)),
            ("Voltage", json!(0)),
            ("IsCharging", json!(false)),
            ("CurrentCapacity", json!(0)),
            ("DesignCapacity", json!(0)),
            ("NominalChargeCapacity", json!(0)),
            ("AbsoluteCapacity", json!(0)),
            ("AppleRawCurrentCapacity", json!(0)),
            ("AppleRawMaxCapacity", json!(0)),
            ("CycleCount", json!(0)),
            ("AtCriticalLevel", json!(false)),
            ("AtWarnLevel", json!(false)),
        ],
    )?;
    Ok(json!({
        "Diagnostics": diagnostics,
        "Status": "Success",
    }))
}

fn ensure_diagnostics_fields(
    diagnostics: &mut serde_json::Map<String, Value>,
    subsystem: &str,
    defaults: &[(&str, Value)],
) -> CoreResult<()> {
    let value = diagnostics
        .entry(subsystem.to_owned())
        .or_insert_with(|| json!({}));
    let fields = value.as_object_mut().ok_or_else(|| {
        CoreError::Backend(format!(
            "diagnostics list {subsystem} value is not an object"
        ))
    })?;
    for (name, default) in defaults {
        fields
            .entry((*name).to_owned())
            .or_insert_with(|| default.clone());
    }
    Ok(())
}

fn plist_value_kind(value: &plist::Value) -> &'static str {
    match value {
        plist::Value::Array(_) => "array",
        plist::Value::Boolean(_) => "bool",
        plist::Value::Data(_) => "data",
        plist::Value::Date(_) => "date",
        plist::Value::Dictionary(_) => "dictionary",
        plist::Value::Integer(_) => "integer",
        plist::Value::Real(_) => "real",
        plist::Value::String(_) => "string",
        _ => "unknown value",
    }
}

async fn command_image(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
        CoreError::InvalidArgument("image requires auto, list, mount or unmount".into())
    })?;
    let mut mounter = idevice::mobile_image_mounter::ImageMounter::connect(provider)
        .await
        .map_err(map_device_error)?;
    // mobile_image_mounter requires a Lockdown query after service startup on some iOS versions.
    let mut lockdown = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    if let Ok(pairing) = provider.get_pairing_file().await {
        let _ = lockdown.start_session(&pairing).await;
    }
    let product_version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await
        .map_err(map_device_error)?
        .as_string()
        .unwrap_or_default()
        .to_owned();
    let major_version = product_version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);

    match action {
        "list" => plist_json(mounter.copy_devices().await.map_err(map_device_error)?),
        "unmount" => {
            let mount_path = if major_version >= 17 {
                "/System/Developer"
            } else {
                "/Developer"
            };
            mounter
                .unmount_image(mount_path)
                .await
                .map_err(map_device_error)?;
            Ok(json!({ "status": "unmounted", "mountPath": mount_path }))
        }
        "mount" | "auto" if major_version >= 17 => {
            let board_id = lockdown_unsigned_value(&mut lockdown, "BoardId").await?;
            let chip_id = lockdown_unsigned_value(&mut lockdown, "ChipID").await?;
            let unique_chip_id = lockdown_unsigned_value(&mut lockdown, "UniqueChipID").await?;
            let ddi = if action == "auto" {
                let base_dir = resolve_client_path(
                    invocation,
                    &option_value(&invocation.argv, "--basedir")
                        .unwrap_or_else(|| "./devimages".into()),
                );
                find_best_personalized_ddi(&base_dir, board_id, chip_id)?.ok_or_else(|| {
                    CoreError::InvalidArgument(format!(
                        "no compatible personalized Developer Disk Image under {}; expected ddi-*/Restore with a BuildIdentity for BoardId 0x{board_id:x} and ChipID 0x{chip_id:x}",
                        base_dir.display()
                    ))
                })?
            } else {
                let raw_path = option_value(&invocation.argv, "--path").ok_or_else(|| {
                    CoreError::InvalidArgument("image mount requires --path".into())
                })?;
                let restore_dir = resolve_client_path(invocation, &raw_path);
                load_personalized_ddi(&restore_dir, board_id, chip_id)?
            };

            let image = tokio::fs::read(&ddi.image_path).await?;
            let trust_cache = tokio::fs::read(&ddi.trust_cache_path).await?;
            mounter
                .mount_personalized(
                    provider,
                    image,
                    trust_cache,
                    &ddi.build_manifest,
                    None,
                    unique_chip_id,
                )
                .await
                .map_err(map_device_error)?;
            Ok(json!({
                "status": "mounted",
                "mode": "personalized",
                "source": action,
                "buildVersion": ddi.build_version,
                "restoreDirectory": ddi.restore_dir,
                "image": ddi.image_path,
                "trustCache": ddi.trust_cache_path,
            }))
        }
        "auto" => {
            let base_dir = resolve_client_path(
                invocation,
                &option_value(&invocation.argv, "--basedir")
                    .unwrap_or_else(|| "./devimages".into()),
            );
            let ddi = download_legacy_ddi(&base_dir, &product_version).await?;
            let image = tokio::fs::read(&ddi.image_path).await?;
            let signature = tokio::fs::read(&ddi.signature_path).await?;
            mounter
                .mount_developer(&image, signature)
                .await
                .map_err(map_device_error)?;
            Ok(json!({
                "status": "mounted",
                "mode": "developer",
                "source": if ddi.downloaded { "download" } else { "cache" },
                "productVersion": product_version,
                "imageVersion": ddi.version,
                "image": ddi.image_path,
                "signature": ddi.signature_path,
            }))
        }
        "mount" => {
            let raw_path = option_value(&invocation.argv, "--path")
                .ok_or_else(|| CoreError::InvalidArgument("image mount requires --path".into()))?;
            let image_path = resolve_client_path(invocation, &raw_path);
            if image_path.extension().and_then(|value| value.to_str()) != Some("dmg") {
                return Err(CoreError::InvalidArgument(
                    "image mount path must be a .dmg file".into(),
                ));
            }
            let mut signature_name = image_path.as_os_str().to_owned();
            signature_name.push(".signature");
            let signature_path = PathBuf::from(signature_name);
            let image = tokio::fs::read(&image_path).await?;
            let signature = tokio::fs::read(&signature_path).await?;
            mounter
                .mount_developer(&image, signature)
                .await
                .map_err(map_device_error)?;
            Ok(json!({
                "status": "mounted",
                "image": image_path,
                "signature": signature_path,
            }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown image action {action:?}"
        ))),
    }
}

async fn lockdown_unsigned_value(
    lockdown: &mut idevice::lockdown::LockdownClient,
    key: &str,
) -> CoreResult<u64> {
    let value = lockdown
        .get_value(Some(key), None)
        .await
        .map_err(map_device_error)?;
    value
        .as_unsigned_integer()
        .or_else(|| {
            value
                .as_signed_integer()
                .and_then(|value| u64::try_from(value).ok())
        })
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "Lockdown {key} must be a non-negative integer, got {}",
                plist_value_kind(&value)
            ))
        })
}

async fn command_process_mutation(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let command = invocation.argv[0].as_str();
    let requested_pid = option_value(&invocation.argv, "--pid")
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| CoreError::InvalidArgument(format!("invalid process ID {value:?}")))
        })
        .transpose()?;
    let mut requested_name = option_value(&invocation.argv, "--process");
    let bundle_id = if command == "kill" {
        positional_value(&invocation.argv, 1)
    } else {
        None
    };

    if let Some(bundle_id) = &bundle_id {
        let mut installation =
            idevice::installation_proxy::InstallationProxyClient::connect(provider)
                .await
                .map_err(map_device_error)?;
        let apps = installation
            .get_apps(Some("Any"), None)
            .await
            .map_err(map_device_error)?;
        let app = apps.get(bundle_id).ok_or_else(|| {
            CoreError::DeviceUnavailable(format!("bundle {bundle_id:?} is not installed"))
        })?;
        requested_name = plist_string(app.as_dictionary(), "CFBundleExecutable");
        if requested_name.is_none() {
            return Err(CoreError::Backend(format!(
                "installed app {bundle_id:?} has no CFBundleExecutable"
            )));
        }
    }
    if requested_pid.is_none() && requested_name.is_none() {
        return Err(CoreError::InvalidArgument(if command == "kill" {
            "kill requires a bundle ID, --pid or --process".into()
        } else {
            "memlimitoff requires --process".into()
        }));
    }

    let mut remote = connect_dvt(provider, tunnel).await?;
    let processes = {
        let mut service = idevice::dvt::device_info::DeviceInfoClient::new(&mut remote)
            .await
            .map_err(map_device_error)?;
        service
            .running_processes()
            .await
            .map_err(map_device_error)?
    };
    let selected = processes
        .into_iter()
        .find(|process| {
            requested_pid.is_some_and(|pid| process.pid == pid)
                || requested_name
                    .as_ref()
                    .is_some_and(|name| process.name == *name || process.real_app_name == *name)
        })
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "running process not found for pid {requested_pid:?} name {requested_name:?}"
            ))
        })?;
    let mut control = idevice::dvt::process_control::ProcessControlClient::new(&mut remote)
        .await
        .map_err(map_device_error)?;
    if command == "kill" {
        control
            .kill_app(u64::from(selected.pid))
            .await
            .map_err(map_device_error)?;
        Ok(json!({
            "status": "killed",
            "pid": selected.pid,
            "process": selected.name,
            "bundleID": bundle_id,
        }))
    } else {
        control
            .disable_memory_limit(u64::from(selected.pid))
            .await
            .map_err(map_device_error)?;
        Ok(json!({
            "status": "disabled",
            "pid": selected.pid,
            "process": selected.name,
        }))
    }
}

async fn command_debug_app_info(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let local = crate::debug::local_debug_app(invocation)?;
    let mut installation = idevice::installation_proxy::InstallationProxyClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let apps = installation
        .get_apps(Some("User"), None)
        .await
        .map_err(map_device_error)?;
    let installed = apps.get(&local.bundle_id).ok_or_else(|| {
        CoreError::DeviceUnavailable(format!(
            "bundle {:?} from {} is not installed on the selected device",
            local.bundle_id,
            local.app_path.display()
        ))
    })?;
    let dictionary = installed.as_dictionary();
    let device_app_path = plist_string(dictionary, "Path").ok_or_else(|| {
        CoreError::Backend(format!("installed app {:?} has no Path", local.bundle_id))
    })?;
    if !device_app_path.starts_with('/') {
        return Err(CoreError::Backend(format!(
            "installed app {:?} has invalid Path {device_app_path:?}",
            local.bundle_id
        )));
    }
    let device_executable = plist_string(dictionary, "CFBundleExecutable").ok_or_else(|| {
        CoreError::Backend(format!(
            "installed app {:?} has no CFBundleExecutable",
            local.bundle_id
        ))
    })?;
    if device_executable.trim().is_empty()
        || device_executable.contains('/')
        || device_executable.contains('\\')
    {
        return Err(CoreError::Backend(format!(
            "installed app {:?} has invalid CFBundleExecutable {device_executable:?}",
            local.bundle_id
        )));
    }
    let device_executable_path = format!(
        "{}/{}",
        device_app_path.trim_end_matches('/'),
        device_executable
    );
    serde_json::to_value(DebugAppInfo {
        bundle_id: local.bundle_id,
        local_app_path: crate::debug::path_string(&local.app_path),
        local_executable_path: crate::debug::path_string(&local.executable_path),
        device_app_path,
        device_executable_path,
    })
    .map_err(|error| CoreError::Backend(format!("serialize debug app info: {error}")))
}

async fn command_springboard(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let command = invocation.argv[0].as_str();
    let app_icon_request = if command == "get-app-icon" {
        Some(parse_app_icon_request(invocation)?)
    } else {
        None
    };
    let mut client = idevice::springboardservices::SpringBoardServicesClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    match command {
        "get-app-icon" => {
            let request = app_icon_request.expect("get-app-icon request was parsed");
            let bytes = client
                .get_icon_pngdata(request.bundle_identifier.clone())
                .await
                .map_err(map_device_error)?;
            write_png_atomic(
                &request.output_path,
                &bytes,
                &format!("SpringBoard icon for {}", request.bundle_identifier),
            )
            .await?;
            Ok(json!({
                "bundleID": request.bundle_identifier,
                "path": request.output_path,
                "bytes": bytes.len(),
            }))
        }
        "get-wallpaper" => {
            let bytes = client
                .get_home_screen_wallpaper_preview_pngdata()
                .await
                .map_err(map_device_error)?;
            let raw_output = option_value(&invocation.argv, "--output")
                .unwrap_or_else(|| "wallpaper.png".into());
            let output = resolve_client_path(invocation, &raw_output);
            if let Some(parent) = output.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&output, &bytes).await?;
            Ok(json!({ "path": output, "bytes": bytes.len() }))
        }
        "get-icon-layout" => {
            let state = client
                .get_icon_state(Some("2"))
                .await
                .map_err(map_device_error)?;
            let value = plist_json(state)?;
            if let Some(raw_output) = option_value(&invocation.argv, "--output") {
                let output = resolve_client_path(invocation, &raw_output);
                if let Some(parent) = output.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let bytes = serde_json::to_vec_pretty(&value)
                    .map_err(|error| CoreError::Backend(error.to_string()))?;
                tokio::fs::write(&output, &bytes).await?;
                Ok(json!({ "path": output, "bytes": bytes.len() }))
            } else {
                Ok(value)
            }
        }
        "set-icon-layout" => {
            let raw_path = positional_value(&invocation.argv, 1).ok_or_else(|| {
                CoreError::InvalidArgument("set-icon-layout requires a layout file".into())
            })?;
            let path = resolve_client_path(invocation, &raw_path);
            let bytes = tokio::fs::read(&path).await?;
            let state: plist::Value = serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidArgument(format!(
                    "icon layout {} is not valid JSON: {error}",
                    path.display()
                ))
            })?;
            client
                .set_icon_state(state)
                .await
                .map_err(map_device_error)?;
            Ok(json!({ "status": "set", "path": path }))
        }
        _ => unreachable!(),
    }
}

async fn command_prepare_host(invocation: &CommandInvocation) -> CoreResult<Option<Value>> {
    match invocation.argv.get(1).map(String::as_str) {
        Some("printskip") => Ok(Some(json!(SETUP_SKIP_KEYS))),
        Some("create-cert") => {
            let password = option_value(&invocation.argv, "--p12password")
                .or_else(|| std::env::var("P12_PASSWORD").ok())
                .unwrap_or_default();
            let identity = generate_identity(&password)
                .map_err(|error| CoreError::Backend(error.to_string()))?;
            let outputs = [
                (
                    "certificateDer",
                    "supervision-cert.der",
                    identity.certificate_der,
                ),
                (
                    "certificatePem",
                    "supervision-cert.pem",
                    identity.certificate_pem,
                ),
                (
                    "privateKeyDer",
                    "supervision-private-key.key",
                    identity.private_key_der,
                ),
                (
                    "privateKeyPem",
                    "supervision-private-key.pem",
                    identity.private_key_pem,
                ),
                ("csr", "supervision-csr.csr", identity.csr_pem),
                ("pkcs12", "supervision-identity.p12", identity.pkcs12_der),
            ];
            let mut paths = serde_json::Map::new();
            for (key, file_name, bytes) in outputs {
                let path = resolve_client_path(invocation, file_name);
                tokio::fs::write(&path, bytes).await?;
                paths.insert(key.into(), json!(path));
            }
            Ok(Some(json!({
                "status": "created",
                "files": paths,
                "pkcs12PasswordProtected": !password.is_empty(),
            })))
        }
        _ => Ok(None),
    }
}

async fn command_profile(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    warnings: &mut Vec<String>,
) -> CoreResult<Value> {
    let action =
        invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
            CoreError::InvalidArgument("profile requires add, list or remove".into())
        })?;
    match action {
        "list" => {
            reject_options(
                &invocation.argv,
                &["--p12file", "--password"],
                "profile list",
            )?;
            let mut client = McInstallClient::connect(provider)
                .await
                .map_err(map_device_error)?;
            client.list_profiles().await.map_err(map_device_error)
        }
        "add" => {
            let raw_path = positional_value(&invocation.argv, 2).ok_or_else(|| {
                CoreError::InvalidArgument("profile add requires a profile file".into())
            })?;
            let path = resolve_client_path(invocation, &raw_path);
            let bytes = tokio::fs::read(&path).await?;
            let p12_path = optional_nonempty_value(&invocation.argv, "--p12file")?;
            let explicit_password = optional_nonempty_value(&invocation.argv, "--password")?;
            if p12_path.is_none() && explicit_password.is_some() {
                return Err(CoreError::InvalidArgument(
                    "profile add --password requires --p12file".into(),
                ));
            }
            let mut client = McInstallClient::connect(provider)
                .await
                .map_err(map_device_error)?;
            if let Some(raw_p12_path) = p12_path {
                let p12_path = resolve_client_path(invocation, &raw_p12_path);
                let p12 = tokio::fs::read(&p12_path).await?;
                let password = explicit_password
                    .or_else(|| std::env::var("P12_PASSWORD").ok())
                    .unwrap_or_default();
                sign_pkcs12_challenge(&p12, &password, b"rust-ios validation")
                    .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
                client
                    .install_profile_silent(bytes, &p12, &password)
                    .await
                    .map_err(map_device_error)?;
                Ok(json!({
                    "status": "installed",
                    "path": path,
                    "silent": true,
                    "supervisionIdentity": p12_path,
                }))
            } else {
                client
                    .install_profile(bytes)
                    .await
                    .map_err(map_device_error)?;
                warnings.push(
                    "profile was submitted with InstallProfile; the device may require manual approval"
                        .into(),
                );
                Ok(json!({ "status": "installed", "path": path, "silent": false }))
            }
        }
        "remove" => {
            reject_options(
                &invocation.argv,
                &["--p12file", "--password"],
                "profile remove",
            )?;
            let identifier = positional_value(&invocation.argv, 2).ok_or_else(|| {
                CoreError::InvalidArgument("profile remove requires an identifier".into())
            })?;
            let mut client = McInstallClient::connect(provider)
                .await
                .map_err(map_device_error)?;
            client
                .remove_profile(&identifier)
                .await
                .map_err(map_device_error)?;
            Ok(json!({ "status": "removed", "identifier": identifier }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown profile action {action:?}"
        ))),
    }
}

async fn command_erase(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    selected_udid: &str,
) -> CoreResult<Value> {
    let requested_udid = validate_erase_invocation(invocation)?;
    if requested_udid != selected_udid {
        return Err(CoreError::Backend(format!(
            "erase selected device {selected_udid}, expected explicit UDID {requested_udid}"
        )));
    }
    let acknowledged = tokio::time::timeout(Duration::from_secs(30), async {
        let mut client = McInstallClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        client.flush().await.map_err(map_device_error)?;
        client
            .get_cloud_configuration()
            .await
            .map_err(map_device_error)?;
        client.erase_device().await.map_err(map_device_error)
    })
    .await
    .map_err(|_| {
        CoreError::Timeout("erase request exceeded 30 seconds; the device state is unknown".into())
    })??;
    Ok(json!({
        "status": "erase_requested",
        "preserveDataPlan": true,
        "response": if acknowledged { "acknowledged" } else { "device_disconnected" },
        "udid": selected_udid,
    }))
}

/// `mdm security-info|fetch-unlock-token|clear-passcode|clear-screen-time-password`
/// （go-ios v1.2.1 8df7d2e/a778c9e）。全部要求 supervision identity
/// （`--p12file`），连接后先 escalation 再执行对应 MCInstall 请求。token
/// 只写入 `--output` 文件（0600）或经 `--output=-` 以 base64 输出到 stdout，
/// 绝不进入结果 JSON/state/日志。
async fn command_mdm(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
        CoreError::InvalidArgument(
            "mdm requires security-info, fetch-unlock-token, clear-passcode or clear-screen-time-password"
                .into(),
        )
    })?;
    let raw_p12_path = optional_nonempty_value(&invocation.argv, "--p12file")?
        .ok_or_else(|| CoreError::InvalidArgument("mdm requires --p12file".into()))?;
    let p12_path = resolve_client_path(invocation, &raw_p12_path);
    let p12 = tokio::fs::read(&p12_path).await.map_err(|error| {
        CoreError::Io(io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", p12_path.display()),
        ))
    })?;
    if p12.len() > MAX_SUPERVISION_IDENTITY_BYTES {
        return Err(CoreError::InvalidArgument(format!(
            "supervision identity {} is {} bytes; limit is {MAX_SUPERVISION_IDENTITY_BYTES}",
            p12_path.display(),
            p12.len()
        )));
    }
    let password = optional_nonempty_value(&invocation.argv, "--password")?
        .or_else(|| std::env::var("P12_PASSWORD").ok())
        .unwrap_or_default();
    // 设备连接前本地验证 identity，避免把无效 P12 发给设备。
    certificate_from_identity(&p12, &password).map_err(|error| {
        CoreError::InvalidArgument(format!(
            "invalid supervision identity {}: {error}",
            p12_path.display()
        ))
    })?;

    let mut client = McInstallClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    client
        .escalate(&p12, &password)
        .await
        .map_err(map_device_error)?;

    match action {
        "security-info" => {
            reject_options(
                &invocation.argv,
                &["--output", "--token"],
                "mdm security-info",
            )?;
            let info = client.security_info().await.map_err(map_device_error)?;
            Ok(json!({
                "securityInfo": info,
                "serviceTransport": "lockdown",
            }))
        }
        "fetch-unlock-token" => {
            let raw_output =
                optional_nonempty_value(&invocation.argv, "--output")?.ok_or_else(|| {
                    CoreError::InvalidArgument("mdm fetch-unlock-token requires --output".into())
                })?;
            let token = client
                .fetch_unlock_token()
                .await
                .map_err(map_device_error)?;
            if raw_output == "-" {
                // base64 stdout 管道输出（go-ios a99646f），token 不落盘。
                return Ok(json!({
                    "status": "fetched",
                    "output": "-",
                    "bytes": token.len(),
                    "tokenBase64": BASE64.encode(token),
                }));
            }
            let output = resolve_client_path(invocation, &raw_output);
            crate::app_store_connect::atomic_write(&output, &token, 0o600)?;
            Ok(json!({
                "status": "fetched",
                "path": output,
                "bytes": token.len(),
            }))
        }
        "clear-passcode" => {
            reject_options(&invocation.argv, &["--output"], "mdm clear-passcode")?;
            let raw_token =
                optional_nonempty_value(&invocation.argv, "--token")?.ok_or_else(|| {
                    CoreError::InvalidArgument("mdm clear-passcode requires --token".into())
                })?;
            let token_input = resolve_client_path(invocation, &raw_token);
            let token = tokio::fs::read(&token_input).await.map_err(|error| {
                CoreError::Io(io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", token_input.display()),
                ))
            })?;
            client
                .clear_passcode(token)
                .await
                .map_err(map_device_error)?;
            Ok(json!({ "status": "cleared", "serviceTransport": "lockdown" }))
        }
        "clear-screen-time-password" => {
            reject_options(
                &invocation.argv,
                &["--output", "--token"],
                "mdm clear-screen-time-password",
            )?;
            client
                .clear_screen_time_password()
                .await
                .map_err(map_device_error)?;
            Ok(json!({ "status": "cleared", "serviceTransport": "lockdown" }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown mdm action {action:?}"
        ))),
    }
}

const MAX_WALLPAPER_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_SUPERVISION_IDENTITY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
enum PreparedPair {
    Standard { host_name: Option<String> },
    Supervised { p12: Vec<u8>, password: String },
}

async fn prepare_pair(invocation: &CommandInvocation) -> CoreResult<PreparedPair> {
    match parse_pair_invocation(invocation)? {
        PairInvocation::Standard { host_name } => Ok(PreparedPair::Standard { host_name }),
        PairInvocation::Supervised { p12_path, password } => {
            let p12_path = resolve_client_path(invocation, &p12_path);
            let p12 = read_bounded_command_file(
                &p12_path,
                "supervision identity",
                MAX_SUPERVISION_IDENTITY_BYTES,
            )
            .await?;
            let password = password
                .or_else(|| {
                    std::env::var("P12_PASSWORD")
                        .ok()
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or_default();
            sign_pkcs12_challenge(&p12, &password, b"rust-ios supervised pair validation")
                .map_err(|error| {
                    CoreError::InvalidArgument(format!(
                        "invalid supervision identity {}: {error}",
                        p12_path.display()
                    ))
                })?;
            Ok(PreparedPair::Supervised { p12, password })
        }
    }
}

#[derive(Debug)]
struct PreparedWallpaper {
    image_path: PathBuf,
    image: Vec<u8>,
    width: u32,
    height: u32,
    screen: WallpaperScreen,
    p12: Vec<u8>,
    password: String,
}

async fn prepare_set_wallpaper(invocation: &CommandInvocation) -> CoreResult<PreparedWallpaper> {
    let SetWallpaperInvocation {
        image_path,
        screen,
        p12_path,
        password,
    } = parse_set_wallpaper_invocation(invocation)?;
    let image_path = resolve_client_path(invocation, &image_path);
    let image =
        read_bounded_command_file(&image_path, "wallpaper image", MAX_WALLPAPER_IMAGE_BYTES)
            .await?;
    let (width, height) = validate_wallpaper_image(&image, &image_path)?;

    let p12_path = resolve_client_path(invocation, &p12_path);
    let p12 = read_bounded_command_file(
        &p12_path,
        "supervision identity",
        MAX_SUPERVISION_IDENTITY_BYTES,
    )
    .await?;
    let password = password
        .or_else(|| {
            std::env::var("P12_PASSWORD")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_default();
    sign_pkcs12_challenge(&p12, &password, b"rust-ios set-wallpaper validation").map_err(
        |error| {
            CoreError::InvalidArgument(format!(
                "invalid supervision identity {}: {error}",
                p12_path.display()
            ))
        },
    )?;

    Ok(PreparedWallpaper {
        image_path,
        image,
        width,
        height,
        screen,
        p12,
        password,
    })
}

async fn command_set_wallpaper(
    provider: &dyn IdeviceProvider,
    udid: &str,
    prepared: PreparedWallpaper,
    warnings: &mut Vec<String>,
) -> CoreResult<Value> {
    let PreparedWallpaper {
        image_path,
        image,
        width,
        height,
        screen,
        p12,
        password,
    } = prepared;
    let image_bytes = image.len();
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut client = McInstallClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        client
            .set_wallpaper_supervised(image, screen, &p12, &password)
            .await
            .map_err(map_device_error)
    })
    .await
    .map_err(|_| {
        CoreError::Timeout(
            "set-wallpaper exceeded 30 seconds; the device wallpaper state is unknown".into(),
        )
    })??;
    if screen != WallpaperScreen::Both {
        warnings.push(
            "iOS 16 and later may apply a wallpaper to both screens regardless of --screen".into(),
        );
    }
    Ok(json!({
        "status": "set",
        "path": image_path,
        "bytes": image_bytes,
        "width": width,
        "height": height,
        "screen": screen.as_str(),
        "where": screen.where_value(),
        "supervised": true,
        "udid": udid,
    }))
}

async fn read_bounded_command_file(
    path: &Path,
    label: &str,
    maximum: usize,
) -> CoreResult<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.map_err(|error| {
        CoreError::InvalidArgument(format!(
            "could not inspect {label} {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "{label} {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > maximum as u64 {
        return Err(CoreError::InvalidArgument(format!(
            "{label} {} exceeds {maximum} bytes",
            path.display()
        )));
    }
    let bytes = tokio::fs::read(path).await.map_err(|error| {
        CoreError::InvalidArgument(format!(
            "could not read {label} {}: {error}",
            path.display()
        ))
    })?;
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(CoreError::InvalidArgument(format!(
            "{label} {} must contain 1..={maximum} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn validate_wallpaper_image(bytes: &[u8], path: &Path) -> CoreResult<(u32, u32)> {
    let format = image::guess_format(bytes).map_err(|error| {
        CoreError::InvalidArgument(format!(
            "wallpaper image {} is not a recognized JPEG or PNG: {error}",
            path.display()
        ))
    })?;
    if !matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::Png) {
        return Err(CoreError::InvalidArgument(format!(
            "wallpaper image {} must be JPEG or PNG, got {format:?}",
            path.display()
        )));
    }
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|error| {
            CoreError::InvalidArgument(format!(
                "wallpaper image {} has an invalid header: {error}",
                path.display()
            ))
        })?;
    if width == 0 || height == 0 {
        return Err(CoreError::InvalidArgument(format!(
            "wallpaper image {} has zero dimensions",
            path.display()
        )));
    }
    Ok((width, height))
}

async fn command_activate(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    udid: &str,
    warnings: &mut Vec<String>,
) -> CoreResult<Value> {
    if invocation.argv.len() != 1 {
        return Err(CoreError::InvalidArgument(
            "activate accepts no command-specific arguments".into(),
        ));
    }
    let initial_state = activation_state(provider).await?;
    if initial_state != "Unactivated" {
        return Ok(json!({
            "status": "already_activated",
            "activationState": initial_state,
            "verified": true,
            "udid": udid,
        }));
    }

    let session_response = mobileactivation_exchange(provider, session_info_request()).await?;
    let session_info =
        response_value_dictionary(&session_response, "CreateTunnel1SessionInfoRequest")?;
    validate_session_info(&session_info)?;
    let http = http_client()?;
    let handshake_response = send_drm_handshake(&http, DRM_HANDSHAKE_URL, &session_info).await?;

    let activation_info_response =
        mobileactivation_exchange(provider, activation_info_request(handshake_response)).await?;
    let activation_info =
        response_value_dictionary(&activation_info_response, "CreateActivationInfoRequest")?;
    let activation_response =
        send_activation_info(&http, DEVICE_ACTIVATION_URL, &activation_info).await?;
    let response_bytes = activation_response.body.len();
    let handle_response = mobileactivation_exchange(
        provider,
        handle_activation_request(activation_response.body, activation_response.headers),
    )
    .await?;
    ensure_response_success(&handle_response, "HandleActivationInfoWithSessionRequest")?;

    let mut final_state = None;
    let mut verification_error = None;
    for attempt in 0..10 {
        match activation_state(provider).await {
            Ok(state) if state != "Unactivated" => {
                final_state = Some(state);
                break;
            }
            Ok(state) => final_state = Some(state),
            Err(error) => verification_error = Some(error.to_string()),
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let verified = final_state
        .as_deref()
        .is_some_and(|state| state != "Unactivated");
    if !verified {
        warnings.push(match verification_error {
            Some(error) => format!(
                "activation response was accepted, but final ActivationState could not be read: {error}"
            ),
            None => {
                "activation response was accepted, but ActivationState is still Unactivated"
                    .into()
            }
        });
    }
    Ok(json!({
        "status": if verified { "activated" } else { "activation_response_accepted" },
        "activationState": final_state,
        "verified": verified,
        "activationResponseBytes": response_bytes,
        "udid": udid,
    }))
}

async fn mobileactivation_exchange(
    provider: &dyn IdeviceProvider,
    request: plist::Dictionary,
) -> CoreResult<plist::Dictionary> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut client = MobileActivationClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        client.exchange(request).await.map_err(map_device_error)
    })
    .await
    .map_err(|_| CoreError::Timeout("mobileactivationd request exceeded 15 seconds".into()))?
}

async fn activation_state(provider: &dyn IdeviceProvider) -> CoreResult<String> {
    tokio::time::timeout(
        Duration::from_secs(5),
        idevice::mobileactivationd::MobileActivationdClient::new(provider).state(),
    )
    .await
    .map_err(|_| CoreError::Timeout("read ActivationState exceeded 5 seconds".into()))?
    .map_err(map_device_error)
}

async fn command_wifi(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    warnings: &mut Vec<String>,
) -> CoreResult<Value> {
    let ssid = optional_nonempty_value(&invocation.argv, "--ssid")?
        .ok_or_else(|| CoreError::InvalidArgument("wifi requires a non-empty --ssid".into()))?;
    let activated = idevice::mobileactivationd::MobileActivationdClient::new(provider)
        .activated()
        .await
        .map_err(map_device_error)?;
    if !activated {
        return Err(CoreError::InvalidArgument(
            "wifi requires an activated device".into(),
        ));
    }
    let mut client = McInstallClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    client.flush().await.map_err(map_device_error)?;
    let cloud = client
        .get_cloud_configuration()
        .await
        .map_err(map_device_error)?;
    let supervised = cloud
        .get("IsSupervised")
        .and_then(plist::Value::as_boolean)
        .unwrap_or(false);
    let _ = client
        .escalate_unsupervised()
        .await
        .map_err(map_device_error)?;

    let identifier = wifi_profile_identifier(&ssid).map_err(map_device_error)?;
    if has_flag(&invocation.argv, "--remove") {
        client
            .remove_profile(&identifier)
            .await
            .map_err(map_device_error)?;
        if !supervised {
            warnings.push(
                "device is not supervised; Wi-Fi profile removal may require manual confirmation"
                    .into(),
            );
        }
        return Ok(json!({
            "status": "removed",
            "ssid": ssid,
            "identifier": identifier,
            "supervised": supervised,
        }));
    }

    let password = optional_nonempty_value(&invocation.argv, "--password")?
        .ok_or_else(|| CoreError::InvalidArgument("wifi requires a non-empty --password".into()))?;
    let encryption_type =
        option_value(&invocation.argv, "--enc-type").unwrap_or_else(|| "WPA".into());
    let (_, profile) =
        wifi_profile(&ssid, &password, &encryption_type).map_err(map_device_error)?;
    client
        .install_profile(profile)
        .await
        .map_err(map_device_error)?;
    if !supervised {
        warnings.push(
            "device is not supervised; approve the Wi-Fi profile in Settings before it takes effect"
                .into(),
        );
    }
    Ok(json!({
        "status": "installed",
        "ssid": ssid,
        "identifier": identifier,
        "encryptionType": encryption_type,
        "supervised": supervised,
    }))
}

async fn command_http_proxy(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    if invocation.argv.get(1).map(String::as_str) == Some("remove") {
        let mut client = McInstallClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        client
            .remove_profile(HTTP_PROXY_PROFILE_IDENTIFIER)
            .await
            .map_err(map_device_error)?;
        return Ok(json!({
            "status": "removed",
            "identifier": HTTP_PROXY_PROFILE_IDENTIFIER,
        }));
    }

    let host = positional_value(&invocation.argv, 1)
        .ok_or_else(|| CoreError::InvalidArgument("httpproxy requires host".into()))?;
    let port = positional_value(&invocation.argv, 2)
        .ok_or_else(|| CoreError::InvalidArgument("httpproxy requires port".into()))?
        .parse::<u16>()
        .map_err(|_| CoreError::InvalidArgument("httpproxy port must be 1..65535".into()))?;
    if port == 0 {
        return Err(CoreError::InvalidArgument(
            "httpproxy port must be 1..65535".into(),
        ));
    }
    let username = positional_value(&invocation.argv, 3);
    let password =
        positional_value(&invocation.argv, 4).or_else(|| std::env::var("PROXY_PASSWORD").ok());
    let raw_p12_path = optional_nonempty_value(&invocation.argv, "--p12file")?
        .ok_or_else(|| CoreError::InvalidArgument("httpproxy requires --p12file".into()))?;
    let p12_path = resolve_client_path(invocation, &raw_p12_path);
    let p12 = tokio::fs::read(&p12_path).await?;
    let p12_password = optional_nonempty_value(&invocation.argv, "--password")?
        .or_else(|| std::env::var("P12_PASSWORD").ok())
        .unwrap_or_default();
    sign_pkcs12_challenge(&p12, &p12_password, b"rust-ios validation")
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let profile = http_proxy_profile(&host, port, username.as_deref(), password.as_deref())
        .map_err(map_device_error)?;
    let mut client = McInstallClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    client
        .install_profile_silent(profile, &p12, &p12_password)
        .await
        .map_err(map_device_error)?;
    Ok(json!({
        "status": "installed",
        "host": host,
        "port": port,
        "authenticated": username.is_some(),
        "identifier": HTTP_PROXY_PROFILE_IDENTIFIER,
        "supervisionIdentity": p12_path,
    }))
}

async fn command_prepare(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    warnings: &mut Vec<String>,
) -> CoreResult<Value> {
    if invocation.argv.get(1).map(String::as_str) == Some("cloudconfig") {
        let mut client = McInstallClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        let configuration = client
            .get_cloud_configuration()
            .await
            .map_err(map_device_error)?;
        return plist_json(plist::Value::Dictionary(configuration));
    }
    if let Some(action) = positional_value(&invocation.argv, 1) {
        return Err(CoreError::InvalidArgument(format!(
            "unknown prepare action {action:?}"
        )));
    }
    let skip_all = has_flag(&invocation.argv, "--skip-all");
    let requested_skips = option_values(&invocation.argv, "--skip");
    if skip_all && !requested_skips.is_empty() {
        return Err(CoreError::InvalidArgument(
            "prepare cannot combine --skip-all with --skip".into(),
        ));
    }
    let skips = if requested_skips.is_empty() {
        SETUP_SKIP_KEYS
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>()
    } else {
        for value in &requested_skips {
            if !SETUP_SKIP_KEYS.contains(&value.as_str()) {
                return Err(CoreError::InvalidArgument(format!(
                    "unknown prepare --skip value {value:?}; use prepare printskip"
                )));
            }
        }
        requested_skips
    };
    let locale = option_value(&invocation.argv, "--locale").unwrap_or_else(|| "en_US".into());
    let language = option_value(&invocation.argv, "--lang").unwrap_or_else(|| "en".into());
    if locale.is_empty() || language.is_empty() {
        return Err(CoreError::InvalidArgument(
            "prepare locale and language cannot be empty".into(),
        ));
    }
    let cert_path = optional_nonempty_value(&invocation.argv, "--certfile")?;
    let organization = optional_nonempty_value(&invocation.argv, "--orgname")?;
    if cert_path.is_some() && organization.is_none() {
        return Err(CoreError::InvalidArgument(
            "prepare --certfile requires --orgname".into(),
        ));
    }
    if cert_path.is_none() && organization.is_some() {
        return Err(CoreError::InvalidArgument(
            "prepare --orgname requires --certfile".into(),
        ));
    }
    let certificate = if let Some(raw_path) = cert_path {
        let path = resolve_client_path(invocation, &raw_path);
        let bytes = tokio::fs::read(&path).await?;
        let password = option_value(&invocation.argv, "--p12password")
            .or_else(|| std::env::var("P12_PASSWORD").ok())
            .unwrap_or_default();
        Some(
            certificate_from_identity(&bytes, &password)
                .map_err(|error| CoreError::InvalidArgument(error.to_string()))?,
        )
    } else {
        None
    };
    let activated = idevice::mobileactivationd::MobileActivationdClient::new(provider)
        .activated()
        .await
        .map_err(map_device_error)?;
    if !activated {
        return Err(CoreError::InvalidArgument(
            "prepare requires an activated device".into(),
        ));
    }

    let mut client = McInstallClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    client.flush().await.map_err(map_device_error)?;
    client
        .get_cloud_configuration()
        .await
        .map_err(map_device_error)?;
    client
        .hello_host_identifier()
        .await
        .map_err(map_device_error)?;
    let mut cloud = plist::Dictionary::new();
    cloud.insert("AllowPairing".into(), true.into());
    cloud.insert(
        "SkipSetup".into(),
        plist::Value::Array(skips.iter().cloned().map(plist::Value::String).collect()),
    );
    if let Some(certificate) = certificate {
        cloud.insert("IsSupervised".into(), true.into());
        cloud.insert("IsMultiUser".into(), false.into());
        cloud.insert(
            "OrganizationName".into(),
            organization.clone().unwrap_or_default().into(),
        );
        cloud.insert(
            "OrganizationMagic".into(),
            uuid::Uuid::new_v4().to_string().into(),
        );
        cloud.insert(
            "SupervisorHostCertificates".into(),
            plist::Value::Array(vec![plist::Value::Data(certificate)]),
        );
    }
    client
        .set_cloud_configuration(cloud)
        .await
        .map_err(map_device_error)?;
    client
        .hello_host_identifier()
        .await
        .map_err(map_device_error)?;
    let final_cloud = client
        .get_cloud_configuration()
        .await
        .map_err(map_device_error)?;
    client
        .hello_host_identifier()
        .await
        .map_err(map_device_error)?;
    let _ = client
        .escalate_unsupervised()
        .await
        .map_err(map_device_error)?;
    client
        .hello_host_identifier()
        .await
        .map_err(map_device_error)?;
    client
        .install_profile(initial_prepare_profile().map_err(map_device_error)?)
        .await
        .map_err(map_device_error)?;
    client
        .hello_host_identifier()
        .await
        .map_err(map_device_error)?;
    drop(client);

    let language_invocation = CommandInvocation {
        argv: vec![
            "lang".into(),
            "--setlocale".into(),
            locale.clone(),
            "--setlang".into(),
            language.clone(),
        ],
        cwd: invocation.cwd.clone(),
        udid: invocation.udid.clone(),
        output: invocation.output.clone(),
    };
    command_language(provider, &language_invocation).await?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let mut lockdown = lockdown_with_session(provider).await?;
    lockdown
        .set_value("TimeIntervalSince1970", plist::Value::Real(now), None)
        .await
        .map_err(map_device_error)?;
    lockdown
        .set_value("TimeZone", "Europe/Berlin".into(), None)
        .await
        .map_err(map_device_error)?;
    drop(lockdown);

    let mut afc = idevice::afc::AfcClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let _ = afc.remove_all("/iTunes_Control/iTunes/SkipSetup").await;
    if let Err(error) = afc.mk_dir("/iTunes_Control/iTunes/SkipSetup").await {
        warnings.push(format!(
            "AFC SkipSetup directory creation returned {error}; continuing with file creation"
        ));
    }
    let mut file = afc
        .open(
            "/iTunes_Control/iTunes/SkipSetup/SkipSetup",
            idevice::afc::opcode::AfcFopenMode::WrOnly,
        )
        .await
        .map_err(map_device_error)?;
    file.write_entire(&[]).await.map_err(map_device_error)?;
    file.close().await.map_err(map_device_error)?;

    Ok(json!({
        "status": "prepared",
        "skip": skips,
        "locale": locale,
        "language": language,
        "supervised": organization.is_some(),
        "organization": organization,
        "cloudConfiguration": plist_json(plist::Value::Dictionary(final_cloud))?,
    }))
}

async fn command_crash(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let action = invocation
        .argv
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument("crash requires ls, cp or rm".into()))?;
    let (start, pattern_text) = match action {
        "ls" => (
            "/".to_owned(),
            positional_value(&invocation.argv, 2).unwrap_or_else(|| "*".into()),
        ),
        "cp" => (
            "/".to_owned(),
            positional_value(&invocation.argv, 2).ok_or_else(|| {
                CoreError::InvalidArgument("crash cp requires a source pattern".into())
            })?,
        ),
        "rm" => (
            normalize_remote_path(&positional_value(&invocation.argv, 2).ok_or_else(|| {
                CoreError::InvalidArgument("crash rm requires a remote directory".into())
            })?),
            positional_value(&invocation.argv, 3)
                .ok_or_else(|| CoreError::InvalidArgument("crash rm requires a pattern".into()))?,
        ),
        _ => {
            return Err(CoreError::InvalidArgument(format!(
                "unknown crash action {action:?}"
            )));
        }
    };
    if pattern_text.is_empty() {
        return Err(CoreError::InvalidArgument(
            "crash pattern cannot be empty; use * for all reports".into(),
        ));
    }
    let pattern = Pattern::new(&pattern_text).map_err(|error| {
        CoreError::InvalidArgument(format!("invalid crash pattern {pattern_text:?}: {error}"))
    })?;

    idevice::crashreportcopymobile::flush_reports(provider)
        .await
        .map_err(map_device_error)?;
    let mut client = idevice::crashreportcopymobile::CrashReportCopyMobileClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let files = walk_crash_files(&mut client.afc_client, &start, &pattern).await?;
    match action {
        "ls" => {
            let length = files.len();
            Ok(json!({ "files": files, "length": length }))
        }
        "cp" => {
            let raw_target = positional_value(&invocation.argv, 3).ok_or_else(|| {
                CoreError::InvalidArgument("crash cp requires a target directory".into())
            })?;
            let target = resolve_client_path(invocation, &raw_target);
            tokio::fs::create_dir_all(&target).await?;
            let mut outputs = Vec::with_capacity(files.len());
            for remote in files {
                let name = Path::new(&remote)
                    .file_name()
                    .ok_or_else(|| CoreError::Backend(format!("invalid crash path {remote:?}")))?;
                let output = target.join(name);
                let bytes = read_afc_file(&mut client.afc_client, &remote).await?;
                tokio::fs::write(&output, &bytes).await?;
                outputs.push(json!({
                    "remote": remote,
                    "local": output,
                    "bytes": bytes.len(),
                }));
            }
            let length = outputs.len();
            Ok(json!({ "files": outputs, "length": length }))
        }
        "rm" => {
            for remote in &files {
                client
                    .afc_client
                    .remove(remote.clone())
                    .await
                    .map_err(map_device_error)?;
            }
            let length = files.len();
            Ok(json!({ "removed": files, "length": length }))
        }
        _ => unreachable!(),
    }
}

async fn walk_crash_files(
    afc: &mut idevice::afc::AfcClient,
    start: &str,
    pattern: &Pattern,
) -> CoreResult<Vec<String>> {
    let start_path = normalize_remote_path(start);
    let mut directories = vec![start_path.clone()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = afc
            .list_dir(directory.clone())
            .await
            .map_err(map_device_error)?;
        for name in entries
            .into_iter()
            .filter(|name| name != "." && name != "..")
        {
            let path = remote_join(&directory, &name)?;
            let Some(info) = afc_file_info(afc, path.clone(), directory != start_path).await?
            else {
                continue;
            };
            if info.st_ifmt == "S_IFDIR" {
                directories.push(path);
            } else if pattern.matches(&name) {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn normalize_remote_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "." || path == "/" {
        "/".into()
    } else {
        format!("/{}", path.trim_matches('/'))
    }
}

fn remote_join(directory: &str, name: &str) -> CoreResult<String> {
    let relative = remote_entry_relative_path(directory, name)?;
    let directory = normalize_remote_path(directory);
    if directory == "/" {
        Ok(format!("/{relative}"))
    } else {
        Ok(format!("{directory}/{relative}"))
    }
}

fn remote_entry_relative_path(directory: &str, name: &str) -> CoreResult<String> {
    let components = remote_entry_components(name)?;
    let directory_components = normalize_remote_path(directory)
        .trim_matches('/')
        .split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let relative =
        if !directory_components.is_empty() && components.starts_with(&directory_components) {
            &components[directory_components.len()..]
        } else {
            &components
        };
    if relative.is_empty() {
        return Err(CoreError::Backend(format!(
            "AFC returned empty directory entry path {name:?}"
        )));
    }
    Ok(relative.join("/"))
}

fn remote_entry_components(name: &str) -> CoreResult<Vec<String>> {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') || name.contains('\0') {
        return Err(CoreError::Backend(format!(
            "AFC returned unsafe directory entry name {name:?}"
        )));
    }
    let normalized = name.replace('\\', "/");
    let components = normalized.split('/').map(str::to_owned).collect::<Vec<_>>();
    if components
        .iter()
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(CoreError::Backend(format!(
            "AFC returned unsafe directory entry name {name:?}"
        )));
    }
    Ok(components)
}

async fn afc_file_info(
    afc: &mut idevice::afc::AfcClient,
    path: String,
    ignore_missing: bool,
) -> CoreResult<Option<idevice::afc::FileInfo>> {
    match afc.get_file_info(path).await {
        Ok(info) => Ok(Some(info)),
        Err(idevice::IdeviceError::Afc(idevice::afc::errors::AfcError::ObjectNotFound))
            if ignore_missing =>
        {
            Ok(None)
        }
        Err(error) => Err(map_device_error(error)),
    }
}
async fn read_afc_file(afc: &mut idevice::afc::AfcClient, path: &str) -> CoreResult<Vec<u8>> {
    let mut file = afc
        .open(path.to_owned(), idevice::afc::opcode::AfcFopenMode::RdOnly)
        .await
        .map_err(map_device_error)?;
    let bytes = file.read_entire().await.map_err(map_device_error)?;
    file.close().await.map_err(map_device_error)?;
    Ok(bytes)
}

async fn command_fsync(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<Value> {
    let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
        CoreError::InvalidArgument("fsync requires pull, push, rm, tree or mkdir".into())
    })?;
    let mut afc = command_afc(provider, invocation).await?;
    match action {
        "tree" => {
            let path = normalize_remote_path(
                &option_value(&invocation.argv, "--path").unwrap_or_else(|| "/".into()),
            );
            let entries = afc_tree(&mut afc, &path).await?;
            let count = entries.len();
            Ok(json!({ "path": path, "entries": entries, "count": count }))
        }
        "mkdir" => {
            let path = required_option_value(&invocation.argv, "--path", "fsync mkdir")?;
            let path = normalize_remote_path(&path);
            afc.mk_dir(path.clone()).await.map_err(map_device_error)?;
            Ok(json!({ "status": "created", "path": path }))
        }
        "rm" => {
            let path = required_option_value(&invocation.argv, "--path", "fsync rm")?;
            let path = normalize_remote_path(&path);
            if path == "/" {
                return Err(CoreError::InvalidArgument(
                    "fsync rm refuses to remove the AFC root".into(),
                ));
            }
            let recursive = has_flag(&invocation.argv, "--r");
            if recursive {
                afc.remove_all(path.clone())
                    .await
                    .map_err(map_device_error)?;
            } else {
                afc.remove(path.clone()).await.map_err(map_device_error)?;
            }
            Ok(json!({ "status": "removed", "path": path, "recursive": recursive }))
        }
        "pull" => {
            let source = required_option_value(&invocation.argv, "--srcPath", "fsync pull")?;
            let source = normalize_remote_path(&source);
            let destination = required_option_value(&invocation.argv, "--dstPath", "fsync pull")?;
            let destination = resolve_client_path(invocation, &destination);
            let source_info = afc
                .get_file_info(source.clone())
                .await
                .map_err(map_device_error)?;
            prepare_fsync_pull_destination(&destination, source_info.st_ifmt == "S_IFDIR").await?;
            let summary = pull_afc_path(&mut afc, &source, &destination).await?;
            Ok(json!({
                "remote": source,
                "local": destination,
                "files": summary.files,
                "directories": summary.directories,
                "bytes": summary.bytes,
            }))
        }
        "push" => {
            let source = required_option_value(&invocation.argv, "--srcPath", "fsync push")?;
            let source = resolve_client_path(invocation, &source);
            let metadata = tokio::fs::metadata(&source).await.map_err(|error| {
                CoreError::Io(std::io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", source.display()),
                ))
            })?;
            if !metadata.is_file() {
                return Err(CoreError::InvalidArgument(format!(
                    "fsync push source {} is not a file",
                    source.display()
                )));
            }
            let destination = required_option_value(&invocation.argv, "--dstPath", "fsync push")?;
            let mut destination = normalize_remote_path(&destination);
            if afc
                .get_file_info(destination.clone())
                .await
                .is_ok_and(|info| info.st_ifmt == "S_IFDIR")
            {
                let name = source.file_name().ok_or_else(|| {
                    CoreError::InvalidArgument(format!(
                        "fsync push source {} has no file name",
                        source.display()
                    ))
                })?;
                let name = name.to_str().ok_or_else(|| {
                    CoreError::InvalidArgument("fsync push source name is not valid UTF-8".into())
                })?;
                destination = remote_join(&destination, name)?;
            }
            let bytes = push_afc_file(&mut afc, &source, &destination).await?;
            Ok(json!({ "local": source, "remote": destination, "bytes": bytes }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown fsync action {action:?}"
        ))),
    }
}

async fn command_file(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let selection = crate::file::parse_selection(&invocation.argv)?;
    if let Some(selection) = selection {
        if let Some(tunnel) = tunnel {
            return crate::file::execute(invocation, tunnel, selection).await;
        }
        if selection.requires_tunnel() {
            return Err(CoreError::DeviceUnavailable(format!(
                "file --{} requires a running server-owned userspace tunnel; run tunnel start --userspace",
                match selection.domain {
                    crate::file::FileDomain::AppGroup => "app-group",
                    crate::file::FileDomain::Temporary => "temp",
                    crate::file::FileDomain::SystemCrashLogs => "crash",
                    crate::file::FileDomain::AppData => "app",
                }
            )));
        }
    }

    let action = invocation
        .argv
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument("file requires ls, pull or push".into()))?;
    let mut afc = command_afc(provider, invocation).await?;
    match action {
        "ls" => {
            let path = option_value(&invocation.argv, "--path").unwrap_or_else(|| "/".into());
            let entries = afc.list_dir(path.clone()).await.map_err(map_device_error)?;
            let mut rows = Vec::new();
            for name in entries
                .into_iter()
                .filter(|name| name != "." && name != "..")
            {
                let full_path = remote_join(&path, &name)?;
                let info = afc.get_file_info(full_path.clone()).await.ok();
                rows.push(match info {
                    Some(info) => json!({
                        "name": name,
                        "path": full_path,
                        "size": info.size,
                        "blocks": info.blocks,
                        "created": info.creation.to_string(),
                        "modified": info.modified.to_string(),
                        "type": info.st_ifmt,
                        "linkTarget": info.st_link_target,
                    }),
                    None => json!({ "name": name, "path": full_path }),
                });
            }
            Ok(Value::Array(rows))
        }
        "pull" => {
            let remote = option_value(&invocation.argv, "--remote")
                .ok_or_else(|| CoreError::InvalidArgument("file pull requires --remote".into()))?;
            let local = option_value(&invocation.argv, "--local")
                .ok_or_else(|| CoreError::InvalidArgument("file pull requires --local".into()))?;
            let output = resolve_client_path(invocation, &local);
            let mut file = afc
                .open(remote.clone(), idevice::afc::opcode::AfcFopenMode::RdOnly)
                .await
                .map_err(map_device_error)?;
            let bytes = file.read_entire().await.map_err(map_device_error)?;
            file.close().await.map_err(map_device_error)?;
            if let Some(parent) = output.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&output, &bytes).await?;
            Ok(json!({ "remote": remote, "local": output, "bytes": bytes.len() }))
        }
        "push" => {
            let local = option_value(&invocation.argv, "--local")
                .ok_or_else(|| CoreError::InvalidArgument("file push requires --local".into()))?;
            let remote = option_value(&invocation.argv, "--remote")
                .ok_or_else(|| CoreError::InvalidArgument("file push requires --remote".into()))?;
            let input = resolve_client_path(invocation, &local);
            let bytes = tokio::fs::read(&input).await?;
            let mut file = afc
                .open(remote.clone(), idevice::afc::opcode::AfcFopenMode::WrOnly)
                .await
                .map_err(map_device_error)?;
            file.write_entire(&bytes).await.map_err(map_device_error)?;
            file.close().await.map_err(map_device_error)?;
            Ok(json!({ "local": input, "remote": remote, "bytes": bytes.len() }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown file action {action:?}"
        ))),
    }
}

/// `pasteboard get|set <text>` via the server-owned userspace tunnel's
/// `com.apple.coredevice.pasteboardservice` (iOS 17+ RemoteXPC). 与 go-ios
/// v1.2.1 `pasteboard` 命令对齐：get 返回快照文本/条目统计，set 覆盖 general
/// pasteboard；错误帧（`{"error": {...}}`）必须作为错误返回而不是静默成功。
async fn command_pasteboard(
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let action = invocation
        .argv
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument("pasteboard requires get or set".into()))?;
    let tunnel = tunnel.ok_or_else(|| {
        CoreError::DeviceUnavailable(
            "pasteboard requires a running server-owned userspace tunnel; run tunnel start --userspace"
                .into(),
        )
    })?;
    tunnel
        .ensure_rsd_service(RsdServiceKind::Pasteboard)
        .await?;
    let stream = tunnel
        .connect_rsd_service(RsdServiceKind::Pasteboard)
        .await?;
    let stream: Box<dyn idevice::ReadWrite> = Box::new(stream);
    let mut client = idevice::core_device::PasteboardServiceClient::from_stream(stream)
        .await
        .map_err(map_device_error)?;
    match action {
        "get" => {
            let snapshot = client
                .get(idevice::core_device::GENERAL_PASTEBOARD)
                .await
                .map_err(map_device_error)?;
            let image = snapshot.image();
            Ok(json!({
                "pasteboard": idevice::core_device::GENERAL_PASTEBOARD,
                "changeCount": snapshot.change_count,
                "text": snapshot.text(),
                "items": snapshot.items.len(),
                "imageUti": image.as_ref().map(|(uti, _)| *uti),
                "imageBytes": image.as_ref().map(|(_, bytes)| bytes.len()),
                "promisedItems": snapshot.promised_items().len(),
                "serviceTransport": "shared_rsd",
            }))
        }
        "set" => {
            // 文本只作为命令参数传入，不回显到结果 JSON，避免剪贴板内容
            // 进入日志/state/recording。
            let text = invocation
                .argv
                .get(2)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| CoreError::InvalidArgument("pasteboard set requires text".into()))?;
            client
                .set_text(text, idevice::core_device::GENERAL_PASTEBOARD)
                .await
                .map_err(map_device_error)?;
            Ok(json!({
                "status": "set",
                "pasteboard": idevice::core_device::GENERAL_PASTEBOARD,
                "serviceTransport": "shared_rsd",
            }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown pasteboard action {action:?}"
        ))),
    }
}

#[derive(Debug, Default)]
struct TransferSummary {
    files: usize,
    directories: usize,
    bytes: u64,
}

async fn afc_tree(afc: &mut idevice::afc::AfcClient, root: &str) -> CoreResult<Vec<Value>> {
    let root_path = normalize_remote_path(root);
    let mut pending = vec![root_path.clone()];
    let mut result = Vec::new();
    while let Some(path) = pending.pop() {
        let Some(info) = afc_file_info(afc, path.clone(), path != root_path).await? else {
            continue;
        };
        let is_directory = info.st_ifmt == "S_IFDIR";
        result.push(json!({
            "path": path,
            "type": info.st_ifmt,
            "size": info.size,
            "blocks": info.blocks,
            "linkTarget": info.st_link_target,
        }));
        if is_directory {
            let mut entries = afc.list_dir(path.clone()).await.map_err(map_device_error)?;
            entries.retain(|name| name != "." && name != "..");
            entries.sort_by(|left, right| right.cmp(left));
            for name in entries {
                pending.push(remote_join(&path, &name)?);
            }
        }
    }
    result.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["path"].as_str().unwrap_or_default())
    });
    Ok(result)
}

async fn pull_afc_path(
    afc: &mut idevice::afc::AfcClient,
    remote_root: &str,
    local_root: &Path,
) -> CoreResult<TransferSummary> {
    let remote_root = normalize_remote_path(remote_root);
    let mut pending = vec![(remote_root.clone(), local_root.to_owned())];
    let mut summary = TransferSummary::default();
    while let Some((remote, local)) = pending.pop() {
        let Some(info) = afc_file_info(afc, remote.clone(), remote != remote_root).await? else {
            continue;
        };
        if info.st_ifmt == "S_IFDIR" {
            tokio::fs::create_dir_all(&local).await?;
            summary.directories = summary.directories.saturating_add(1);
            let mut entries = afc
                .list_dir(remote.clone())
                .await
                .map_err(map_device_error)?;
            entries.retain(|name| name != "." && name != "..");
            entries.sort_by(|left, right| right.cmp(left));
            for name in entries {
                let relative = remote_entry_relative_path(&remote, &name)?;
                pending.push((remote_join(&remote, &name)?, local.join(relative)));
            }
        } else {
            if let Some(parent) = local.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let bytes = pull_afc_file(afc, &remote, &local).await?;
            summary.files = summary.files.saturating_add(1);
            summary.bytes = summary.bytes.saturating_add(bytes);
        }
    }
    Ok(summary)
}

async fn pull_afc_file(
    afc: &mut idevice::afc::AfcClient,
    remote: &str,
    local: &Path,
) -> CoreResult<u64> {
    let mut source = afc
        .open(
            remote.to_owned(),
            idevice::afc::opcode::AfcFopenMode::RdOnly,
        )
        .await
        .map_err(map_device_error)?;
    let mut destination = tokio::fs::File::create(local).await?;
    let bytes = tokio::io::copy(&mut source, &mut destination).await?;
    source.close().await.map_err(map_device_error)?;
    Ok(bytes)
}

async fn push_afc_file(
    afc: &mut idevice::afc::AfcClient,
    local: &Path,
    remote: &str,
) -> CoreResult<u64> {
    let mut source = tokio::fs::File::open(local).await?;
    let mut destination = afc
        .open(
            remote.to_owned(),
            idevice::afc::opcode::AfcFopenMode::WrOnly,
        )
        .await
        .map_err(map_device_error)?;
    let bytes = tokio::io::copy(&mut source, &mut destination).await?;
    destination.close().await.map_err(map_device_error)?;
    Ok(bytes)
}

fn required_option_value(arguments: &[String], name: &str, operation: &str) -> CoreResult<String> {
    let value = option_value(arguments, name)
        .ok_or_else(|| CoreError::InvalidArgument(format!("{operation} requires {name}")))?;
    if value.is_empty() || value.starts_with('-') {
        return Err(CoreError::InvalidArgument(format!(
            "{operation} requires a non-empty {name} value"
        )));
    }
    Ok(value)
}

fn one_shot_location_spec(invocation: &CommandInvocation) -> LocationSimulationSpec {
    LocationSimulationSpec {
        udid: invocation.udid.clone(),
        source_path: format!("<{}>", invocation.argv[0]),
        points: Vec::new(),
        output_path: None,
    }
}

fn parse_location_coordinate(
    value: &str,
    minimum: f64,
    maximum: f64,
    label: &str,
) -> CoreResult<f64> {
    let parsed = value.parse::<f64>().map_err(|_| {
        CoreError::InvalidArgument(format!("{label} must be a number, got {value:?}"))
    })?;
    if !parsed.is_finite() || !(minimum..=maximum).contains(&parsed) {
        return Err(CoreError::InvalidArgument(format!(
            "{label} must be {minimum}..{maximum}, got {value:?}"
        )));
    }
    Ok(parsed)
}

async fn prepare_fsync_pull_destination(
    destination: &Path,
    source_is_directory: bool,
) -> CoreResult<()> {
    if source_is_directory {
        tokio::fs::create_dir_all(destination).await?;
    } else if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

#[cfg(test)]
fn validate_single_remote_entry_name(name: &str) -> CoreResult<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(CoreError::Backend(format!(
            "AFC returned unsafe directory entry name {name:?}"
        )));
    }
    Ok(())
}
#[cfg(test)]
fn validate_remote_entry_name(name: &str) -> CoreResult<()> {
    validate_single_remote_entry_name(name)
}

async fn command_afc(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<idevice::afc::AfcClient> {
    // 注意：--app-group/--crash/--temp 的 domain 选择由 command_file 的
    // parse_selection + FileService 路径处理，不会到达这里（v0.1 的
    // not_implemented gates 已随 FileService 迁移移除）。
    if let Some(bundle_id) = option_value(&invocation.argv, "--app") {
        let house = idevice::house_arrest::HouseArrestClient::connect(provider)
            .await
            .map_err(map_device_error)?;
        house
            .vend_container(bundle_id)
            .await
            .map_err(map_device_error)
    } else {
        idevice::afc::AfcClient::connect(provider)
            .await
            .map_err(map_device_error)
    }
}

#[async_trait]
impl DeviceCondition for IdeviceCondition {
    fn info(&self) -> &DeviceConditionInfo {
        &self.info
    }

    async fn close(&self) -> CoreResult<()> {
        let Some(mut remote) = self.remote.lock().await.take() else {
            return Ok(());
        };
        let mut control = idevice::dvt::condition_inducer::ConditionInducerClient::new(&mut remote)
            .await
            .map_err(map_device_error)?;
        control.disable_condition().await.map_err(map_device_error)
    }
}

#[async_trait]
impl DeviceLocationSimulation for IdeviceLocationSimulation {
    fn info(&self) -> &LocationSimulationInfo {
        &self.info
    }

    async fn set_location(&mut self, latitude: f64, longitude: f64) -> CoreResult<()> {
        match self.transport.as_mut().ok_or_else(|| {
            CoreError::DeviceUnavailable("location simulation session is closed".into())
        })? {
            IdeviceLocationTransport::Dvt(remote) => {
                let mut client =
                    idevice::dvt::location_simulation::LocationSimulationClient::new(remote)
                        .await
                        .map_err(map_device_error)?;
                client
                    .set(latitude, longitude)
                    .await
                    .map_err(map_device_error)
            }
            IdeviceLocationTransport::Classic(provider) => {
                let mut client =
                    idevice::simulate_location::LocationSimulationService::connect(provider)
                        .await
                        .map_err(map_device_error)?;
                client
                    .set(&latitude.to_string(), &longitude.to_string())
                    .await
                    .map_err(map_device_error)
            }
        }
    }

    async fn clear(&mut self) -> CoreResult<()> {
        match self.transport.as_mut().ok_or_else(|| {
            CoreError::DeviceUnavailable("location simulation session is closed".into())
        })? {
            IdeviceLocationTransport::Dvt(remote) => {
                let mut client =
                    idevice::dvt::location_simulation::LocationSimulationClient::new(remote)
                        .await
                        .map_err(map_device_error)?;
                client.clear().await.map_err(map_device_error)
            }
            IdeviceLocationTransport::Classic(provider) => {
                let mut client =
                    idevice::simulate_location::LocationSimulationService::connect(provider)
                        .await
                        .map_err(map_device_error)?;
                client.clear().await.map_err(map_device_error)
            }
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.transport.take();
        Ok(())
    }
}

#[async_trait]
impl WdaSession for IdeviceWdaSession {
    fn info(&self) -> &WdaSessionInfo {
        &self.info
    }

    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if self
                    .runner
                    .as_ref()
                    .is_some_and(|runner| !runner.is_finished())
                {
                    Err(CoreError::Backend(
                        "WDA event channel closed while runner is active".into(),
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn status(&mut self) -> CoreResult<Value> {
        if self.runner.as_ref().is_none_or(JoinHandle::is_finished) {
            let result = match self.runner.take() {
                Some(runner) => runner.await,
                None => {
                    return Err(CoreError::DeviceUnavailable(
                        "WDA runner is not active".into(),
                    ));
                }
            };
            return Err(map_wda_runner_result(result));
        }
        let status = idevice::wda::WdaClient::new(&*self.provider)
            .with_ports(self.ports)
            .status()
            .await
            .map_err(map_device_error)?;
        self.info.status = status.clone();
        Ok(status)
    }

    async fn close(&mut self) -> CoreResult<()> {
        if let Some(runner) = self.runner.take() {
            runner.abort();
            let _ = runner.await;
        }
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        Ok(())
    }
}

#[async_trait]
impl UiRunnerSession for IdeviceUiRunnerSession {
    fn info(&self) -> &UiRunnerSessionInfo {
        &self.info
    }

    async fn ensure_running(&mut self) -> CoreResult<()> {
        if self.runner.as_ref().is_none_or(JoinHandle::is_finished) {
            let result = match self.runner.take() {
                Some(runner) => runner.await,
                None => {
                    return Err(CoreError::DeviceUnavailable(
                        "UI runner is not active".into(),
                    ));
                }
            };
            return Err(map_ui_runner_result(result));
        }
        Ok(())
    }

    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => {
                if self
                    .runner
                    .as_ref()
                    .is_some_and(|runner| !runner.is_finished())
                {
                    Err(CoreError::Backend(
                        "UI runner event channel closed while runner is active".into(),
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        if let Some(runner) = self.runner.take() {
            runner.abort();
            let _ = runner.await;
        }
        Ok(())
    }
}

#[async_trait]
impl XCTestSession for IdeviceXCTestSession {
    fn info(&self) -> &XCTestSessionInfo {
        &self.info
    }

    async fn next_update(&mut self) -> CoreResult<XCTestSessionUpdate> {
        if self.runner.as_ref().is_none_or(JoinHandle::is_finished) {
            if let Ok(event) = self.events.try_recv() {
                return Ok(XCTestSessionUpdate::Event(event));
            }
            let runner = self.runner.take().ok_or_else(|| {
                CoreError::Backend("XCTest session ended without a summary".into())
            })?;
            return runner
                .await
                .map_err(|error| CoreError::Backend(format!("XCTest task failed: {error}")))?
                .map(XCTestSessionUpdate::Finished);
        }

        enum Next {
            Event(Option<XCTestEvent>),
            Finished(Result<CoreResult<XCTestRunSummary>, tokio::task::JoinError>),
        }
        let next = {
            let runner = self.runner.as_mut().expect("checked active XCTest task");
            tokio::select! {
                biased;
                event = self.events.recv() => Next::Event(event),
                result = runner => Next::Finished(result),
            }
        };
        match next {
            Next::Event(Some(event)) => Ok(XCTestSessionUpdate::Event(event)),
            Next::Event(None) => {
                let runner = self.runner.take().ok_or_else(|| {
                    CoreError::Backend("XCTest session ended without a summary".into())
                })?;
                runner
                    .await
                    .map_err(|error| CoreError::Backend(format!("XCTest task failed: {error}")))?
                    .map(XCTestSessionUpdate::Finished)
            }
            Next::Finished(result) => {
                self.runner.take();
                result
                    .map_err(|error| CoreError::Backend(format!("XCTest task failed: {error}")))?
                    .map(XCTestSessionUpdate::Finished)
            }
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        if let Some(runner) = self.runner.take() {
            runner.abort();
            let _ = runner.await;
        }
        Ok(())
    }
}

#[async_trait]
impl AppProcessSession for IdeviceAppProcessSession {
    fn info(&self) -> &AppProcessSessionInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Option<Vec<u8>>> {
        let mut buffer = vec![0_u8; 32 * 1024];
        let count = match self.stdio.inner.read(&mut buffer).await {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                if self.owned_process_is_running().await? {
                    return Err(CoreError::Io(error));
                }
                return Ok(None);
            }
            Err(error) => return Err(CoreError::Io(error)),
        };
        if count == 0 {
            return Ok(None);
        }
        buffer.truncate(count);
        Ok(Some(buffer))
    }

    async fn close(&mut self) -> CoreResult<()> {
        if self.app.is_none() {
            return Ok(());
        }
        if self.owned_process_is_running().await? {
            self.app
                .as_mut()
                .expect("checked app service client")
                .send_signal(self.info.pid, 9)
                .await
                .map_err(map_device_error)?;
        }
        self.app.take();
        Ok(())
    }
}

fn map_wda_runner_result(
    result: Result<Result<(), idevice::IdeviceError>, tokio::task::JoinError>,
) -> CoreError {
    match result {
        Ok(Ok(())) => CoreError::DeviceUnavailable("WDA runner exited unexpectedly".into()),
        Ok(Err(error)) => map_device_error(error),
        Err(error) => CoreError::Backend(format!("WDA runner task failed: {error}")),
    }
}

fn map_ui_runner_result(
    result: Result<Result<(), idevice::IdeviceError>, tokio::task::JoinError>,
) -> CoreError {
    match result {
        Ok(Ok(())) => CoreError::DeviceUnavailable("UI runner exited unexpectedly".into()),
        Ok(Err(error)) => map_device_error(error),
        Err(error) => CoreError::Backend(format!("UI runner task failed: {error}")),
    }
}

fn has_flag(arguments: &[String], name: &str) -> bool {
    arguments.iter().any(|value| value == name)
}

fn launch_flag(arguments: &[String], name: &str) -> bool {
    let mut index = 2;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--arg" | "--env" => index += 2,
            value if value == name => return true,
            _ => index += 1,
        }
    }
    false
}

fn reject_options(arguments: &[String], names: &[&str], context: &str) -> CoreResult<()> {
    for name in names {
        let prefix = format!("{name}=");
        if arguments
            .iter()
            .any(|value| value == name || value.starts_with(&prefix))
        {
            return Err(CoreError::InvalidArgument(format!(
                "{context} does not accept {name}"
            )));
        }
    }
    Ok(())
}

fn option_value(arguments: &[String], name: &str) -> Option<String> {
    for (index, value) in arguments.iter().enumerate() {
        if let Some(value) = value.strip_prefix(&(name.to_owned() + "=")) {
            return Some(value.to_owned());
        }
        if value == name {
            return arguments.get(index + 1).cloned();
        }
    }
    None
}

fn option_values(arguments: &[String], name: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let value = &arguments[index];
        if let Some(value) = value.strip_prefix(&(name.to_owned() + "=")) {
            result.push(value.to_owned());
        } else if value == name
            && let Some(value) = arguments.get(index + 1)
        {
            result.push(value.clone());
            index += 1;
        }
        index += 1;
    }
    result
}

fn positional_value(arguments: &[String], index: usize) -> Option<String> {
    arguments
        .get(index)
        .filter(|value| !value.starts_with('-'))
        .cloned()
}

fn parse_key_value_options(arguments: &[String], name: &str) -> CoreResult<plist::Dictionary> {
    let mut result = plist::Dictionary::new();
    for value in option_values(arguments, name) {
        let (key, value) = value.split_once('=').ok_or_else(|| {
            CoreError::InvalidArgument(format!("{name} expects KEY=VALUE, got {value:?}"))
        })?;
        if key.is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "{name} key cannot be empty"
            )));
        }
        result.insert(key.to_owned(), plist::Value::String(value.to_owned()));
    }
    Ok(result)
}

pub(crate) fn resolve_client_path(invocation: &CommandInvocation, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else if let Some(cwd) = &invocation.cwd {
        Path::new(cwd).join(path)
    } else {
        path.to_owned()
    }
}

fn plist_json<T: Serialize>(value: T) -> CoreResult<Value> {
    serde_json::to_value(value).map_err(|error| CoreError::Backend(error.to_string()))
}

fn optional_dictionary_json(value: Option<plist::Dictionary>) -> CoreResult<Value> {
    match value {
        Some(value) => plist_json(value),
        None => Ok(Value::Null),
    }
}

fn plist_string(dictionary: Option<&plist::Dictionary>, key: &str) -> Option<String> {
    dictionary
        .and_then(|dictionary| dictionary.get(key))
        .and_then(plist::Value::as_string)
        .map(str::to_owned)
}

fn raw_device_info(device: UsbmuxdDevice) -> DeviceInfo {
    let (connection_type, network_address, label) = match device.connection_type {
        Connection::Usb => (ConnectionType::Usb, None, "USB".to_owned()),
        Connection::Network(address) => (
            ConnectionType::Network,
            Some(address.to_string()),
            "Network".to_owned(),
        ),
        Connection::Unknown(description) => (
            ConnectionType::Unknown,
            None,
            format!("Unknown:{description}"),
        ),
    };
    DeviceInfo {
        device_id: device.device_id,
        udid: device.udid,
        connection_type,
        network_address,
        properties: [("ConnectionType".into(), label)].into_iter().collect(),
    }
}

fn map_usbmuxd_error(error: idevice::IdeviceError) -> CoreError {
    CoreError::UsbmuxdUnavailable(error.to_string())
}

fn map_device_error(error: idevice::IdeviceError) -> CoreError {
    let message = error.to_string();
    // 设备锁定（passcode screen）时 Lockdown 返回 DeviceLocked/PasswordProtected；
    // 这是可恢复的明确状态，不是泛化设备不可用（go-ios v1.2.1 区分两者，
    // 配对被推迟时 exit 2）。
    if matches!(
        error,
        idevice::IdeviceError::DeviceLocked | idevice::IdeviceError::PasswordProtected
    ) {
        CoreError::DeviceLocked(message)
    } else if message.to_ascii_lowercase().contains("pair") {
        CoreError::PairingRequired(message)
    } else {
        CoreError::DeviceUnavailable(message)
    }
}

fn map_pairing_error(udid: &str, error: idevice::IdeviceError) -> CoreError {
    CoreError::PairingRequired(format!("{udid}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockDeviceBackend;

    #[derive(Debug)]
    struct TestRsdTunnel {
        info: UserSpaceTunnelInfo,
        services: Option<BTreeMap<String, RsdServiceInfo>>,
        connect_error: bool,
    }

    impl TestRsdTunnel {
        fn with_services(services: BTreeMap<String, RsdServiceInfo>) -> Self {
            Self {
                info: UserSpaceTunnelInfo {
                    udid: "phone".into(),
                    client_address: "fd00::1".into(),
                    server_address: "fd00::2".into(),
                    netmask: "ffff:ffff:ffff:ffff::".into(),
                    mtu: 1_500,
                    rsd_port: 58_783,
                },
                services: Some(services),
                connect_error: false,
            }
        }
    }

    #[async_trait]
    impl UserSpaceTunnel for TestRsdTunnel {
        fn info(&self) -> &UserSpaceTunnelInfo {
            &self.info
        }

        async fn connect(&self, _port: u16) -> CoreResult<BoxedIo> {
            if self.connect_error {
                Err(CoreError::DeviceUnavailable(
                    "test shared RSD connect failed".into(),
                ))
            } else {
                panic!("test tunnel connect must not be called")
            }
        }

        async fn rsd_services(&self) -> CoreResult<BTreeMap<String, RsdServiceInfo>> {
            self.services.clone().ok_or_else(|| {
                CoreError::DeviceUnavailable("test shared RSD registry failed".into())
            })
        }

        async fn connect_rsd_service(&self, _service: RsdServiceKind) -> CoreResult<BoxedIo> {
            panic!("named XCTest adapter connects by advertised port")
        }

        async fn close(&self) -> CoreResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn shared_xctest_rsd_adapter_looks_up_named_service_and_forwards_socket_io() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("phone").await;
        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(
            backend
                .start_userspace_tunnel(&TunnelSpec {
                    udid: Some("phone".into()),
                    mode: rust_ios_protocol::TunnelMode::Userspace,
                    transport: TunnelTransport::UsbCoreDevice,
                    pair_record_path: "default".into(),
                    userspace_bind_host: "127.0.0.1".into(),
                    userspace_port: 0,
                    kernel_interface_name: None,
                    wintun_path: None,
                })
                .await
                .unwrap(),
        );
        assert!(
            tunnel
                .rsd_services()
                .await
                .unwrap()
                .contains_key("com.apple.dt.testmanagerd.remote")
        );

        let provider = SharedXCTestRsdProvider { tunnel };
        let mut socket = idevice::dvt::xctest::XCTestRsdProvider::connect_to_service(
            &provider,
            "com.apple.dt.testmanagerd.remote",
        )
        .await
        .unwrap();
        socket.write_all(b"shared-rsd").await.unwrap();
        let mut response = [0_u8; 10];
        socket.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"shared-rsd");
    }

    #[tokio::test]
    async fn shared_xctest_rsd_adapter_rejects_missing_and_oversized_service_ports() {
        let missing = SharedXCTestRsdProvider {
            tunnel: Arc::new(TestRsdTunnel::with_services(BTreeMap::new())),
        };
        let error = idevice::dvt::xctest::XCTestRsdProvider::connect_to_service(
            &missing,
            "missing.service",
        )
        .await
        .unwrap_err();
        assert!(matches!(error, idevice::IdeviceError::ServiceNotFound));

        let oversized = SharedXCTestRsdProvider {
            tunnel: Arc::new(TestRsdTunnel::with_services(BTreeMap::from([(
                "oversized.service".into(),
                RsdServiceInfo { port: 65_536 },
            )]))),
        };
        let error = idevice::dvt::xctest::XCTestRsdProvider::connect_to_service(
            &oversized,
            "oversized.service",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            idevice::IdeviceError::UnexpectedResponse(message)
                if message.contains("invalid port 65536")
        ));
    }

    #[tokio::test]
    async fn shared_xctest_rsd_adapter_maps_registry_and_connect_errors() {
        let registry_failure = SharedXCTestRsdProvider {
            tunnel: Arc::new(TestRsdTunnel {
                services: None,
                ..TestRsdTunnel::with_services(BTreeMap::new())
            }),
        };
        let error = idevice::dvt::xctest::XCTestRsdProvider::connect_to_service(
            &registry_failure,
            "service",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            idevice::IdeviceError::UnknownErrorType(message)
                if message.contains("load shared RSD services for XCTest")
        ));

        let connect_failure = SharedXCTestRsdProvider {
            tunnel: Arc::new(TestRsdTunnel {
                connect_error: true,
                ..TestRsdTunnel::with_services(BTreeMap::from([(
                    "service".into(),
                    RsdServiceInfo { port: 58_793 },
                )]))
            }),
        };
        let error = idevice::dvt::xctest::XCTestRsdProvider::connect_to_service(
            &connect_failure,
            "service",
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            idevice::IdeviceError::UnknownErrorType(message)
                if message.contains("connect shared RSD service")
        ));
    }

    #[test]
    fn shared_xctest_rsd_requires_matching_tunnel_device() {
        let tunnel: Arc<dyn UserSpaceTunnel> =
            Arc::new(TestRsdTunnel::with_services(BTreeMap::new()));
        let error = shared_xctest_rsd(Some(tunnel), "another-phone").unwrap_err();
        assert!(matches!(error, CoreError::InvalidArgument(_)));

        let (provider, transport) = shared_xctest_rsd(None, "phone").unwrap();
        assert!(provider.is_none());
        assert_eq!(transport, ServiceTransport::DedicatedCoreDevice);
    }

    #[cfg(windows)]
    fn kernel_spec(wintun_path: &str) -> TunnelSpec {
        TunnelSpec {
            udid: Some("phone".into()),
            mode: rust_ios_protocol::TunnelMode::Kernel,
            transport: TunnelTransport::UsbCoreDevice,
            pair_record_path: "default".into(),
            userspace_bind_host: "127.0.0.1".into(),
            userspace_port: 0,
            kernel_interface_name: Some("rust-ios-test".into()),
            wintun_path: Some(wintun_path.into()),
        }
    }

    #[cfg(windows)]
    #[test]
    fn missing_wintun_path_is_a_structured_argument_error() {
        let error = resolve_wintun_path(&kernel_spec("C:\\rust-ios-tests\\missing-wintun.dll"))
            .unwrap_err();
        assert!(matches!(error, CoreError::InvalidArgument(_)));
        assert!(error.to_string().contains("missing-wintun.dll"));
    }

    #[cfg(windows)]
    #[test]
    fn permission_probe_never_returns_an_unstructured_io_error() {
        if let Err(error) = ensure_windows_kernel_permissions() {
            assert!(matches!(error, CoreError::PermissionDenied(_)));
            assert!(error.to_string().contains("administrator"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn kernel_packet_validation_checks_version_length_and_mtu() {
        let mut valid = vec![0_u8; 44];
        valid[0] = 0x60;
        valid[4..6].copy_from_slice(&4_u16.to_be_bytes());
        validate_ipv6_packet(&valid, 1280).unwrap();

        let mut wrong_version = valid.clone();
        wrong_version[0] = 0x40;
        assert!(validate_ipv6_packet(&wrong_version, 1280).is_err());
        assert!(validate_ipv6_packet(&valid[..43], 1280).is_err());
        assert!(validate_ipv6_packet(&valid, 43).is_err());
    }

    #[test]
    fn repeated_options_and_environment_are_parsed() {
        let arguments = vec![
            "launch".into(),
            "com.example.App".into(),
            "--arg=first".into(),
            "--arg".into(),
            "second".into(),
            "--env=NAME=value".into(),
            "--env".into(),
            "EMPTY=".into(),
        ];
        assert_eq!(option_values(&arguments, "--arg"), ["first", "second"]);
        let environment = parse_key_value_options(&arguments, "--env").unwrap();
        assert_eq!(environment["NAME"].as_string(), Some("value"));
        assert_eq!(environment["EMPTY"].as_string(), Some(""));

        let argument_named_like_flags = vec![
            "launch".into(),
            "com.example.App".into(),
            "--arg".into(),
            "--wait".into(),
            "--arg=--kill-existing".into(),
        ];
        assert!(!launch_flag(&argument_named_like_flags, "--wait"));
        assert!(!launch_flag(&argument_named_like_flags, "--kill-existing"));
    }

    #[test]
    fn device_date_response_matches_go_ios_shape_and_rejects_invalid_values() {
        let (value, plain) = device_date_response(plist::Value::Real(1_700_000_000.75)).unwrap();
        assert_eq!(value["TimeIntervalSince1970"], 1_700_000_000.75);
        assert_eq!(value["formatedDate"], plain);
        assert_eq!(plain, "Tuesday, 14-Nov-23 22:13:20 UTC");

        let (integer, _) = device_date_response(plist::Value::Integer(42_i64.into())).unwrap();
        assert_eq!(integer["TimeIntervalSince1970"], 42.0);
        assert!(device_date_response(plist::Value::Real(f64::NAN)).is_err());
        assert!(device_date_response(plist::Value::String("now".into())).is_err());
    }

    #[test]
    fn one_shot_screenshot_is_routed_through_shared_rsd() {
        let invocation = CommandInvocation {
            argv: vec!["screenshot".into(), "--output=capture.png".into()],
            cwd: None,
            udid: Some("phone".into()),
            output: Default::default(),
        };
        assert!(command_uses_shared_rsd(&invocation));
    }

    #[test]
    fn one_shot_location_is_routed_through_shared_rsd_and_validates_coordinates() {
        for command in ["setlocation", "resetlocation"] {
            let invocation = CommandInvocation {
                argv: vec![command.into()],
                cwd: None,
                udid: Some("phone".into()),
                output: Default::default(),
            };
            assert!(command_uses_shared_rsd(&invocation));
        }
        assert_eq!(
            parse_location_coordinate("31.2304", -90.0, 90.0, "latitude").unwrap(),
            31.2304
        );
        assert!(parse_location_coordinate("91", -90.0, 90.0, "latitude").is_err());
        assert!(parse_location_coordinate("NaN", -180.0, 180.0, "longitude").is_err());
    }

    #[test]
    fn diagnostics_list_matches_go_ios_envelope_and_requires_subcommand() {
        validate_diagnostics_list(&["diagnostics".into(), "list".into()]).unwrap();
        assert!(validate_diagnostics_list(&["diagnostics".into()]).is_err());
        assert!(validate_diagnostics_list(&["diagnostics".into(), "all".into()]).is_err());

        let value = diagnostics_list_response(Some(plist::Dictionary::from_iter([(
            "GasGauge".to_owned(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        )])))
        .unwrap();
        assert_eq!(value["Status"], "Success");
        assert!(value["Diagnostics"]["GasGauge"].is_object());
        assert_eq!(value["Diagnostics"]["WiFi"]["Active"], "");
        assert_eq!(value["Diagnostics"]["IORegistry"]["CycleCount"], 0);
        assert_eq!(value["Diagnostics"]["IORegistry"]["IsCharging"], false);
        assert!(diagnostics_list_response(None).is_err());
    }

    #[test]
    fn sysmontap_unwraps_nested_batches_and_uses_reference_config() {
        let mut sample = plist::Dictionary::new();
        sample.insert(
            "SystemCPUUsage".into(),
            plist::Value::Dictionary(plist::Dictionary::new()),
        );
        let nested =
            plist::Value::Array(vec![plist::Value::Array(vec![plist::Value::Dictionary(
                sample,
            )])]);
        assert!(
            find_sysmontap_sample(nested)
                .unwrap()
                .contains_key("SystemCPUUsage")
        );

        let config = sysmontap_config(vec!["pid".into(), "name".into()], vec!["vmFree".into()]);
        assert_eq!(config["ur"].as_signed_integer(), Some(10));
        assert_eq!(
            config["sampleInterval"].as_signed_integer(),
            Some(500_000_000)
        );
        assert_eq!(config["procAttrs"].as_array().unwrap().len(), 2);
        assert_eq!(config["sysAttrs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn ostrace_combines_level_subsystem_and_message_filters() {
        let entry = idevice::os_trace_relay::OsTraceLog {
            pid: 42,
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0)
                .unwrap()
                .naive_utc(),
            level: idevice::os_trace_relay::LogLevel::Error,
            image_name: "Demo".into(),
            filename: "/Applications/Demo".into(),
            message: "connection timeout on endpoint".into(),
            label: Some(idevice::os_trace_relay::SyslogLabel {
                subsystem: "com.example.network".into(),
                category: "request".into(),
            }),
        };
        let options = OsTraceOptions {
            levels: vec![OsTraceLevel::Error, OsTraceLevel::Fault],
            subsystem_contains: Some("com.example".into()),
            message_contains: Some("timeout".into()),
            message_excludes: Some("ignored".into()),
            follow: true,
        };
        assert!(os_trace_entry_matches(&entry, &options));

        let mut mismatch = options.clone();
        mismatch.levels = vec![OsTraceLevel::Debug];
        assert!(!os_trace_entry_matches(&entry, &mismatch));
        mismatch = options.clone();
        mismatch.subsystem_contains = Some("com.apple".into());
        assert!(!os_trace_entry_matches(&entry, &mismatch));
        mismatch = options.clone();
        mismatch.message_excludes = Some("timeout".into());
        assert!(!os_trace_entry_matches(&entry, &mismatch));

        let mut unlabeled = entry;
        unlabeled.label = None;
        assert!(!os_trace_entry_matches(&unlabeled, &options));
        assert!(os_trace_entry_matches(
            &unlabeled,
            &OsTraceOptions::default()
        ));
    }

    #[test]
    fn icon_layout_json_deserializes_as_plist_value() {
        let layout: plist::Value = serde_json::from_str(
            r#"[[{"bundleIdentifier":"com.example.App","displayName":"App"}]]"#,
        )
        .unwrap();
        assert!(layout.as_array().is_some());
    }

    #[test]
    fn wallpaper_validation_accepts_png_and_jpeg_and_rejects_other_data() {
        assert_eq!(
            validate_wallpaper_image(
                crate::springboard::MOCK_APP_ICON_PNG,
                Path::new("wallpaper.png")
            )
            .unwrap(),
            (1, 1)
        );

        let image = image::DynamicImage::new_rgb8(3, 2);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .unwrap();
        assert_eq!(
            validate_wallpaper_image(&jpeg, Path::new("wallpaper.jpg")).unwrap(),
            (3, 2)
        );
        assert!(validate_wallpaper_image(b"GIF89a invalid", Path::new("wallpaper.gif")).is_err());
        assert!(validate_wallpaper_image(b"", Path::new("empty.png")).is_err());
    }

    #[test]
    fn lockdown_toggle_accepts_bool_and_integer_zero_or_one() {
        assert!(lockdown_toggle_value(&plist::Value::Boolean(true), "key").unwrap());
        assert!(!lockdown_toggle_value(&plist::Value::Boolean(false), "key").unwrap());
        assert!(!lockdown_toggle_value(&plist::Value::Integer(0_u64.into()), "key").unwrap());
        assert!(lockdown_toggle_value(&plist::Value::Integer(1_u64.into()), "key").unwrap());
        assert!(lockdown_toggle_value(&plist::Value::Integer(1_i64.into()), "key").unwrap());
        assert!(lockdown_toggle_value(&plist::Value::Integer(2_u64.into()), "key").is_err());
        assert!(lockdown_toggle_value(&plist::Value::String("true".into()), "key").is_err());
    }

    #[test]
    fn language_writes_are_preflighted_against_device_supported_values() {
        let current = LanguageConfiguration {
            language: "zh-Hans-CN".into(),
            locale: "zh_CN".into(),
            supported_locales: vec!["zh_CN".into(), "en_US".into()],
            supported_languages: vec!["zh-Hans".into(), "en-US".into()],
        };
        validate_language_request(&current, Some("en_US"), Some("en-US")).unwrap();
        assert!(validate_language_request(&current, None, Some("zh-Hans-CN")).is_err());
        assert!(validate_language_request(&current, Some("invalid_LOCALE"), None).is_err());
    }

    #[test]
    fn lockdown_toggle_actions_are_command_specific() {
        assert_eq!(
            lockdown_toggle_spec("assistivetouch", "enable")
                .unwrap()
                .action,
            ToggleAction::Set(true)
        );
        assert_eq!(
            lockdown_toggle_spec("timeformat", "12h").unwrap().action,
            ToggleAction::Set(false)
        );
        assert!(lockdown_toggle_spec("timeformat", "enable").is_err());
        assert!(lockdown_toggle_spec("voiceover", "24h").is_err());
        assert_eq!(ios_major_version("18.5.1").unwrap(), 18);
        assert!(ios_major_version("not-a-version").is_err());
    }

    #[tokio::test]
    async fn fsync_pull_preserves_go_ios_destination_semantics() {
        let temporary = tempfile::tempdir().unwrap();
        let file_destination = temporary.path().join("nested").join("renamed.bin");
        prepare_fsync_pull_destination(&file_destination, false)
            .await
            .unwrap();
        assert!(file_destination.parent().unwrap().is_dir());
        assert!(!file_destination.exists());

        let directory_destination = temporary.path().join("copied-tree");
        prepare_fsync_pull_destination(&directory_destination, true)
            .await
            .unwrap();
        assert!(directory_destination.is_dir());
    }

    #[test]
    fn fsync_paths_reject_unsafe_remote_entries() {
        assert!(validate_remote_entry_name("IMG_0001.JPG").is_ok());
        assert!(validate_remote_entry_name("..").is_err());
        assert!(validate_remote_entry_name("nested/name").is_err());
        assert!(validate_remote_entry_name("nested\\name").is_err());
    }

    #[test]
    fn fsync_paths_normalize_nested_remote_entries_without_allowing_traversal() {
        let nested = "PublicStaging\\MPBBank.app\\zh-Hans.lproj\\Localizable.strings";
        assert_eq!(
            remote_join("/", nested).unwrap(),
            "/PublicStaging/MPBBank.app/zh-Hans.lproj/Localizable.strings"
        );
        assert_eq!(
            remote_entry_relative_path("/PublicStaging", nested).unwrap(),
            "MPBBank.app/zh-Hans.lproj/Localizable.strings"
        );
        assert!(remote_join("/", "../escape").is_err());
        assert!(remote_join("/", "/absolute").is_err());
    }

    #[test]
    fn ip_parser_matches_source_mac_and_vlan_frames() {
        let mac = parse_mac_address("02:11:22:33:44:55").unwrap();
        assert_eq!(mac, [0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert!(parse_mac_address("not-a-mac").is_err());

        let mut ipv4 = vec![0_u8; 20];
        ipv4[0] = 0x45;
        ipv4[12..16].copy_from_slice(&[192, 0, 2, 7]);
        let mut frame = vec![0_u8; 6];
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&0x0800_u16.to_be_bytes());
        frame.extend_from_slice(&ipv4);
        assert_eq!(
            packet_source_ip(&frame, mac),
            Some("192.0.2.7".parse().unwrap())
        );
        assert_eq!(packet_source_ip(&frame, [0; 6]), None);

        let mut ipv6 = vec![0_u8; 40];
        ipv6[0] = 0x60;
        ipv6[8..24].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]);
        let mut vlan_frame = vec![0_u8; 6];
        vlan_frame.extend_from_slice(&mac);
        vlan_frame.extend_from_slice(&0x8100_u16.to_be_bytes());
        vlan_frame.extend_from_slice(&[0, 1]);
        vlan_frame.extend_from_slice(&0x86dd_u16.to_be_bytes());
        vlan_frame.extend_from_slice(&ipv6);
        assert_eq!(
            packet_source_ip(&vlan_frame, mac),
            Some("2001:db8::7".parse().unwrap())
        );
    }
}
