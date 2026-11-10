use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult, ConnectionType,
    DeviceInfo, DeviceList, DeviceStateSpec, LocationSimulationPoint, LocationSimulationSpec,
    OsTraceLevel, OsTraceOptions, RemotePairingInfo, StreamFormat, StreamTaskSpec, TaskKind,
    TunnelPairSpec, TunnelSpec, TunnelTransport, UiRunnerSpec, WdaSpec, WebInspectorCdpSpec,
    WebInspectorJsShellSpec, XCTestEvent, XCTestEventKind, XCTestRunSummary, XCTestSpec,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::management::{
    PairInvocation, SETUP_SKIP_KEYS, parse_pair_invocation, parse_set_wallpaper_invocation,
    validate_erase_invocation,
};
use crate::springboard::{MOCK_APP_ICON_PNG, parse_app_icon_request, write_png_atomic};
use crate::{
    AppProcessSession, AppProcessSessionInfo, BoxedIo, CoreError, CoreResult, DeviceBackend,
    DeviceCondition, DeviceConditionInfo, DeviceLocationSimulation, DeviceStream, DeviceStreamInfo,
    DeviceTarget, KernelTunnelHandle, KernelTunnelInfo, KernelTunnelRunner, LocationSimulationInfo,
    RsdServiceInfo, RsdServiceKind, ServiceTransport, UiRunnerSession, UiRunnerSessionInfo,
    UserSpaceTunnel, UserSpaceTunnelInfo, WdaSession, WdaSessionInfo, WebInspectorCdpSession,
    WebInspectorCdpSessionInfo, WebInspectorJsShellSession, WebInspectorJsShellSessionInfo,
    WebInspectorPageInfo, XCTestSession, XCTestSessionInfo, XCTestSessionUpdate, select_device,
};

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

/// 无设备测试使用的内存 backend。每个 service connection 都是一个双向 echo 通道。
#[derive(Debug, Clone)]
pub struct MockDeviceBackend {
    inner: Arc<MockInner>,
}

#[derive(Debug)]
struct MockInner {
    devices: RwLock<Vec<DeviceInfo>>,
    unavailable: AtomicBool,
    connect_fail: AtomicBool,
    connect_count: AtomicUsize,
    tunnel_count: AtomicUsize,
    kernel_tunnel_count: AtomicUsize,
    kernel_tunnel_close_count: AtomicUsize,
    debug_proxy_available: AtomicBool,
    condition_count: AtomicUsize,
    condition_close_count: AtomicUsize,
    location_session_count: AtomicUsize,
    location_clear_count: AtomicUsize,
    location_points: RwLock<Vec<LocationSimulationPoint>>,
    wda_count: AtomicUsize,
    wda_close_count: AtomicUsize,
    ui_runner_count: AtomicUsize,
    ui_runner_active_count: AtomicUsize,
    ui_runner_close_count: AtomicUsize,
    xctest_count: AtomicUsize,
    app_process_count: AtomicUsize,
    app_process_close_count: AtomicUsize,
    webinspector_cdp_count: AtomicUsize,
    webinspector_cdp_close_count: AtomicUsize,
    webinspector_js_shell_count: AtomicUsize,
    webinspector_js_shell_close_count: AtomicUsize,
}

impl Default for MockDeviceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDeviceBackend {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MockInner {
                devices: RwLock::new(Vec::new()),
                unavailable: AtomicBool::new(false),
                connect_fail: AtomicBool::new(false),
                connect_count: AtomicUsize::new(0),
                tunnel_count: AtomicUsize::new(0),
                kernel_tunnel_count: AtomicUsize::new(0),
                kernel_tunnel_close_count: AtomicUsize::new(0),
                debug_proxy_available: AtomicBool::new(true),
                condition_count: AtomicUsize::new(0),
                condition_close_count: AtomicUsize::new(0),
                location_session_count: AtomicUsize::new(0),
                location_clear_count: AtomicUsize::new(0),
                location_points: RwLock::new(Vec::new()),
                wda_count: AtomicUsize::new(0),
                wda_close_count: AtomicUsize::new(0),
                ui_runner_count: AtomicUsize::new(0),
                ui_runner_active_count: AtomicUsize::new(0),
                ui_runner_close_count: AtomicUsize::new(0),
                xctest_count: AtomicUsize::new(0),
                app_process_count: AtomicUsize::new(0),
                app_process_close_count: AtomicUsize::new(0),
                webinspector_cdp_count: AtomicUsize::new(0),
                webinspector_cdp_close_count: AtomicUsize::new(0),
                webinspector_js_shell_count: AtomicUsize::new(0),
                webinspector_js_shell_close_count: AtomicUsize::new(0),
            }),
        }
    }

    pub async fn attach_usb(&self, udid: impl Into<String>) {
        let udid = udid.into();
        let mut devices = self.inner.devices.write().await;
        if devices.iter().any(|device| device.udid == udid) {
            return;
        }
        let device_id = devices.len() as u32 + 1;
        devices.push(DeviceInfo {
            device_id,
            udid,
            connection_type: ConnectionType::Usb,
            network_address: None,
            properties: BTreeMap::from([
                ("ConnectionType".into(), "USB".into()),
                ("Mock".into(), "true".into()),
            ]),
        });
    }

    pub async fn attach_network(&self, udid: impl Into<String>, address: impl Into<String>) {
        let udid = udid.into();
        let mut devices = self.inner.devices.write().await;
        let device_id = devices.len() as u32 + 1;
        devices.push(DeviceInfo {
            device_id,
            udid,
            connection_type: ConnectionType::Network,
            network_address: Some(address.into()),
            properties: BTreeMap::from([
                ("ConnectionType".into(), "Network".into()),
                ("Mock".into(), "true".into()),
            ]),
        });
    }

    pub async fn detach(&self, udid: &str) {
        self.inner
            .devices
            .write()
            .await
            .retain(|device| device.udid != udid);
    }

    pub fn set_unavailable(&self, unavailable: bool) {
        self.inner.unavailable.store(unavailable, Ordering::SeqCst);
    }

    pub fn set_connect_failure(&self, fail: bool) {
        self.inner.connect_fail.store(fail, Ordering::SeqCst);
    }

    pub fn connect_count(&self) -> usize {
        self.inner.connect_count.load(Ordering::SeqCst)
    }

    pub fn tunnel_count(&self) -> usize {
        self.inner.tunnel_count.load(Ordering::SeqCst)
    }

    pub fn kernel_tunnel_count(&self) -> usize {
        self.inner.kernel_tunnel_count.load(Ordering::SeqCst)
    }

    pub fn kernel_tunnel_close_count(&self) -> usize {
        self.inner.kernel_tunnel_close_count.load(Ordering::SeqCst)
    }

    pub fn set_debug_proxy_available(&self, available: bool) {
        self.inner
            .debug_proxy_available
            .store(available, Ordering::SeqCst);
    }

    pub fn condition_count(&self) -> usize {
        self.inner.condition_count.load(Ordering::SeqCst)
    }

    pub fn condition_close_count(&self) -> usize {
        self.inner.condition_close_count.load(Ordering::SeqCst)
    }

    pub fn location_session_count(&self) -> usize {
        self.inner.location_session_count.load(Ordering::SeqCst)
    }

    pub fn location_clear_count(&self) -> usize {
        self.inner.location_clear_count.load(Ordering::SeqCst)
    }

    pub async fn location_points(&self) -> Vec<LocationSimulationPoint> {
        self.inner.location_points.read().await.clone()
    }

    pub fn wda_count(&self) -> usize {
        self.inner.wda_count.load(Ordering::SeqCst)
    }

    pub fn wda_close_count(&self) -> usize {
        self.inner.wda_close_count.load(Ordering::SeqCst)
    }

    pub fn ui_runner_count(&self) -> usize {
        self.inner.ui_runner_count.load(Ordering::SeqCst)
    }

    pub fn ui_runner_close_count(&self) -> usize {
        self.inner.ui_runner_close_count.load(Ordering::SeqCst)
    }

    pub fn xctest_count(&self) -> usize {
        self.inner.xctest_count.load(Ordering::SeqCst)
    }

    pub fn app_process_count(&self) -> usize {
        self.inner.app_process_count.load(Ordering::SeqCst)
    }

    pub fn app_process_close_count(&self) -> usize {
        self.inner.app_process_close_count.load(Ordering::SeqCst)
    }

    pub fn webinspector_cdp_count(&self) -> usize {
        self.inner.webinspector_cdp_count.load(Ordering::SeqCst)
    }

    pub fn webinspector_cdp_close_count(&self) -> usize {
        self.inner
            .webinspector_cdp_close_count
            .load(Ordering::SeqCst)
    }

    pub fn webinspector_js_shell_count(&self) -> usize {
        self.inner
            .webinspector_js_shell_count
            .load(Ordering::SeqCst)
    }

    pub fn webinspector_js_shell_close_count(&self) -> usize {
        self.inner
            .webinspector_js_shell_close_count
            .load(Ordering::SeqCst)
    }

    fn ensure_available(&self) -> CoreResult<()> {
        if self.inner.unavailable.load(Ordering::SeqCst) {
            Err(CoreError::UsbmuxdUnavailable(
                "mock backend is configured as unavailable".into(),
            ))
        } else {
            Ok(())
        }
    }

    async fn echo_stream(&self) -> CoreResult<BoxedIo> {
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock service connection failure".into(),
            ));
        }
        self.inner.connect_count.fetch_add(1, Ordering::SeqCst);
        let (client, mut service) = tokio::io::duplex(256 * 1024);
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 32 * 1024];
            loop {
                let count = match service.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                if service.write_all(&buffer[..count]).await.is_err() {
                    break;
                }
            }
        });
        Ok(Box::new(client))
    }

    async fn ui_runner_stream(&self) -> CoreResult<BoxedIo> {
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock service connection failure".into(),
            ));
        }
        self.inner.connect_count.fetch_add(1, Ordering::SeqCst);
        let (client, mut service) = tokio::io::duplex(256 * 1024);
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 32 * 1024];
            let count = match service.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            if buffer[..count].starts_with(b"GET ") {
                let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"ready\":true,\"mock\":true}";
                let _ = service.write_all(response).await;
                return;
            }
            if service.write_all(&buffer[..count]).await.is_err() {
                return;
            }
            loop {
                let count = match service.read(&mut buffer).await {
                    Ok(0) | Err(_) => return,
                    Ok(count) => count,
                };
                if service.write_all(&buffer[..count]).await.is_err() {
                    return;
                }
            }
        });
        Ok(Box::new(client))
    }
}

#[async_trait]
impl DeviceBackend for MockDeviceBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn list_devices(&self) -> CoreResult<DeviceList> {
        self.ensure_available()?;
        Ok(DeviceList {
            devices: self.inner.devices.read().await.clone(),
        })
    }

    async fn read_buid(&self) -> CoreResult<String> {
        self.ensure_available()?;
        Ok("MOCK-BUID-00000000".into())
    }

    async fn read_pair_record(&self, udid: &str) -> CoreResult<Value> {
        self.ensure_available()?;
        let target = DeviceTarget {
            udid: Some(udid.into()),
            ..DeviceTarget::default()
        };
        self.resolve_device(&target).await?;
        Ok(json!({
            "HostID": "mock-host",
            "SystemBUID": "MOCK-BUID-00000000",
            "UDID": udid,
            "Mock": true
        }))
    }

    async fn connect_port(&self, target: &DeviceTarget, port: u16) -> CoreResult<BoxedIo> {
        self.ensure_available()?;
        let devices = self.inner.devices.read().await.clone();
        select_device(devices, target)?;
        if matches!(port, 8100 | 12004)
            && self.inner.ui_runner_active_count.load(Ordering::SeqCst) > 0
        {
            self.ui_runner_stream().await
        } else {
            self.echo_stream().await
        }
    }

    async fn start_userspace_tunnel(
        &self,
        spec: &TunnelSpec,
    ) -> CoreResult<Box<dyn UserSpaceTunnel>> {
        self.ensure_available()?;
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        if spec.transport == TunnelTransport::RemotePairingTcp
            && !Path::new(&spec.pair_record_path)
                .join("selfIdentity.plist")
                .is_file()
        {
            return Err(CoreError::PairingRequired(format!(
                "mock RemotePairing identity is missing under {}",
                spec.pair_record_path
            )));
        }
        let device = select_device(self.inner.devices.read().await.clone(), &target)?;
        self.inner.tunnel_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockTunnel {
            backend: self.clone(),
            info: UserSpaceTunnelInfo {
                udid: device.udid,
                client_address: "fd00::1".into(),
                server_address: "fd00::2".into(),
                netmask: "ffff:ffff:ffff:ffff::".into(),
                mtu: 16_000,
                rsd_port: 58_783,
            },
            closed: AtomicBool::new(false),
        }))
    }

    async fn start_kernel_tunnel(&self, spec: &TunnelSpec) -> CoreResult<KernelTunnelHandle> {
        self.ensure_available()?;
        if spec.transport != TunnelTransport::UsbCoreDevice {
            return Err(CoreError::InvalidArgument(
                "mock kernel tunnel only supports UsbCoreDevice transport".into(),
            ));
        }
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = select_device(self.inner.devices.read().await.clone(), &target)?;
        self.inner
            .kernel_tunnel_count
            .fetch_add(1, Ordering::SeqCst);
        let interface_name = spec
            .kernel_interface_name
            .clone()
            .unwrap_or_else(|| "rust-ios-mock".into());
        let route_info = UserSpaceTunnelInfo {
            udid: device.udid.clone(),
            client_address: "fd00::1".into(),
            server_address: "fd00::2".into(),
            netmask: "ffff:ffff:ffff:ffff::".into(),
            mtu: 16_000,
            rsd_port: 58_783,
        };
        let route: Arc<dyn UserSpaceTunnel> = Arc::new(MockTunnel {
            backend: self.clone(),
            info: route_info.clone(),
            closed: AtomicBool::new(false),
        });
        Ok(KernelTunnelHandle {
            info: KernelTunnelInfo {
                udid: device.udid,
                interface_name,
                client_address: route_info.client_address,
                server_address: route_info.server_address,
                netmask: route_info.netmask,
                mtu: route_info.mtu,
                rsd_port: route_info.rsd_port,
            },
            route,
            runner: Box::new(MockKernelTunnelRunner {
                backend: self.clone(),
                closed: false,
            }),
        })
    }

    async fn pair_remote_tunnel(&self, spec: &TunnelPairSpec) -> CoreResult<RemotePairingInfo> {
        self.ensure_available()?;
        if !spec.manual {
            return Err(CoreError::InvalidArgument(
                "RemotePairing requires explicit manual authorization".into(),
            ));
        }
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = select_device(self.inner.devices.read().await.clone(), &target)?;
        let directory = Path::new(&spec.pair_record_path);
        std::fs::create_dir_all(directory)?;
        let identity_path = directory.join("selfIdentity.plist");
        let newly_paired = !identity_path.exists();
        std::fs::write(&identity_path, b"mock-remote-pairing-identity")?;
        Ok(RemotePairingInfo {
            udid: device.udid,
            identity_path: identity_path.to_string_lossy().into_owned(),
            device_address: "fe80::2%1".into(),
            newly_paired,
        })
    }

    async fn start_stream(
        &self,
        spec: &StreamTaskSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock stream connection failure".into(),
            ));
        }
        let target = DeviceTarget {
            udid: spec.udid.clone(),
            ..DeviceTarget::default()
        };
        let device = select_device(self.inner.devices.read().await.clone(), &target)?;
        let info = match spec.kind {
            TaskKind::Syslog
            | TaskKind::OsTrace
            | TaskKind::Notifications
            | TaskKind::Sysmontap => DeviceStreamInfo {
                content_type: match spec.format {
                    StreamFormat::JsonLines => "application/x-ndjson",
                    _ => "text/plain; charset=utf-8",
                }
                .into(),
                initial_bytes: Vec::new(),
                service_transport: mock_service_transport(&spec.kind, tunnel.as_ref()),
            },
            TaskKind::Pcap if spec.format == StreamFormat::Pcap => DeviceStreamInfo {
                content_type: "application/vnd.tcpdump.pcap".into(),
                initial_bytes: pcap_header(),
                service_transport: ServiceTransport::Mock,
            },
            TaskKind::ScreenshotStream if spec.format == StreamFormat::Raw => DeviceStreamInfo {
                content_type: "image/jpeg".into(),
                initial_bytes: Vec::new(),
                service_transport: mock_service_transport(&spec.kind, tunnel.as_ref()),
            },
            TaskKind::Pcap => {
                return Err(CoreError::InvalidArgument(
                    "pcap stream requires pcap format".into(),
                ));
            }
            ref kind => {
                return Err(CoreError::NotImplemented(format!(
                    "mock stream kind {:?} is not implemented",
                    kind
                )));
            }
        };
        Ok(Box::new(MockDeviceStream {
            info,
            kind: spec.kind.clone(),
            format: spec.format.clone(),
            os_trace: spec.os_trace.clone().unwrap_or_default(),
            udid: device.udid,
            sequence: 0,
            closed: false,
        }))
    }

    async fn start_accessibility_inspector(
        &self,
        spec: &AccessibilityInspectorSpec,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock Accessibility inspector connection failure".into(),
            ));
        }
        let device = select_device(
            self.inner.devices.read().await.clone(),
            &DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            },
        )?;
        Ok(Box::new(MockAccessibilityInspectorStream {
            info: DeviceStreamInfo {
                content_type: "application/x-ndjson".into(),
                initial_bytes: Vec::new(),
                service_transport: ServiceTransport::Mock,
            },
            udid: device.udid,
            font_size: spec.font_size,
            sequence: 0,
            queued_start: true,
            closed: false,
        }))
    }

    async fn start_device_condition(
        &self,
        spec: &DeviceStateSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceCondition>> {
        self.ensure_available()?;
        if spec.condition_identifier.is_empty() || spec.profile_identifier.is_empty() {
            return Err(CoreError::InvalidArgument(
                "device condition and profile identifiers cannot be empty".into(),
            ));
        }
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        self.inner.condition_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockDeviceCondition {
            backend: self.clone(),
            info: DeviceConditionInfo {
                udid: device.udid,
                condition_identifier: spec.condition_identifier.clone(),
                profile_identifier: spec.profile_identifier.clone(),
                profile_description: "Mock device condition".into(),
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            closed: AtomicBool::new(false),
        }))
    }

    async fn start_location_simulation(
        &self,
        spec: &LocationSimulationSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceLocationSimulation>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock location simulation connection failure".into(),
            ));
        }
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        self.inner
            .location_session_count
            .fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockLocationSimulation {
            backend: self.clone(),
            info: LocationSimulationInfo {
                udid: device.udid,
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            closed: false,
        }))
    }

    async fn start_wda_session(
        &self,
        spec: &WdaSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WdaSession>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock WDA connection failure".into(),
            ));
        }
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        self.inner.wda_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockWdaSession {
            backend: self.clone(),
            info: WdaSessionInfo {
                udid: device.udid,
                wda_url: "http://127.0.0.1:18100".into(),
                mjpeg_url: "http://127.0.0.1:19100".into(),
                device_http_port: 8100,
                device_mjpeg_port: 9100,
                status: json!({ "ready": true, "mock": true }),
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            events: VecDeque::from([XCTestEvent {
                timestamp_ms: unix_time_ms(),
                target_index: 0,
                target_name: spec.runner_bundle_id.clone(),
                event: XCTestEventKind::TestPlanStarted,
            }]),
            closed: false,
        }))
    }

    async fn start_ui_runner_session(
        &self,
        spec: &UiRunnerSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn UiRunnerSession>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock UI runner connection failure".into(),
            ));
        }
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        self.inner.ui_runner_count.fetch_add(1, Ordering::SeqCst);
        self.inner
            .ui_runner_active_count
            .fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockUiRunnerSession {
            backend: self.clone(),
            info: UiRunnerSessionInfo {
                udid: device.udid,
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            events: VecDeque::from([XCTestEvent {
                timestamp_ms: unix_time_ms(),
                target_index: 0,
                target_name: spec.target.as_str().into(),
                event: XCTestEventKind::Log {
                    message: "mock UI runner started".into(),
                    debug: false,
                },
            }]),
            closed: false,
        }))
    }

    async fn start_xctest_session(
        &self,
        spec: &XCTestSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn XCTestSession>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock XCTest connection failure".into(),
            ));
        }
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        self.inner.xctest_count.fetch_add(1, Ordering::SeqCst);
        let started_at_ms = unix_time_ms();
        let mut updates = VecDeque::new();
        for (target_index, target) in spec.targets.iter().enumerate() {
            let (test_class, method) = mock_xctest_case(target);
            let event = |event| {
                XCTestSessionUpdate::Event(XCTestEvent {
                    timestamp_ms: unix_time_ms(),
                    target_index,
                    target_name: target.name.clone(),
                    event,
                })
            };
            updates.push_back(event(XCTestEventKind::TargetStarted));
            updates.push_back(event(XCTestEventKind::TestPlanStarted));
            updates.push_back(event(XCTestEventKind::SuiteStarted {
                suite: target.name.clone(),
                started_at: "mock".into(),
            }));
            updates.push_back(event(XCTestEventKind::CaseStarted {
                test_class: test_class.clone(),
                method: method.clone(),
            }));
            updates.push_back(event(XCTestEventKind::CaseFinished {
                test_class,
                method,
                status: "passed".into(),
                duration: 0.001,
            }));
            updates.push_back(event(XCTestEventKind::SuiteFinished {
                suite: target.name.clone(),
                finished_at: "mock".into(),
                run_count: 1,
                failures: 0,
                unexpected: 0,
                test_duration: 0.001,
                total_duration: 0.001,
                skipped: 0,
                expected_failures: 0,
                uncaught_exceptions: 0,
            }));
            updates.push_back(event(XCTestEventKind::TestPlanFinished));
            updates.push_back(event(XCTestEventKind::TargetFinished {
                outcome: "passed".into(),
            }));
        }
        let finished_at_ms = unix_time_ms();
        updates.push_back(XCTestSessionUpdate::Finished(XCTestRunSummary {
            started_at_ms,
            finished_at_ms,
            duration_ms: finished_at_ms.saturating_sub(started_at_ms),
            target_count: spec.targets.len(),
            completed_targets: spec.targets.len(),
            suite_count: spec.targets.len() as u64,
            test_count: spec.targets.len() as u64,
            passed: spec.targets.len() as u64,
            failed: 0,
            skipped: 0,
            expected_failures: 0,
            stalled: 0,
        }));
        Ok(Box::new(MockXCTestSession {
            info: XCTestSessionInfo {
                udid: device.udid,
                target_count: spec.targets.len(),
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            updates,
            closed: false,
        }))
    }

    async fn start_app_process_session(
        &self,
        spec: &AppLaunchSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn AppProcessSession>> {
        self.ensure_available()?;
        if self.inner.connect_fail.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable(
                "mock app process connection failure".into(),
            ));
        }
        let tunnel = tunnel.ok_or_else(|| {
            CoreError::DeviceUnavailable(
                "launch --wait requires a running server-owned userspace tunnel; run tunnel start --userspace"
                    .into(),
            )
        })?;
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        if tunnel.info().udid != device.udid {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }
        self.inner.app_process_count.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockAppProcessSession {
            backend: self.clone(),
            info: AppProcessSessionInfo {
                udid: device.udid,
                bundle_id: spec.bundle_id.clone(),
                pid: 42_424,
                service_transport: ServiceTransport::SharedRsd,
            },
            chunks: VecDeque::from([b"mock app stdout\n".to_vec(), b"mock app stderr\n".to_vec()]),
            closed: false,
        }))
    }

    async fn start_webinspector_cdp_session(
        &self,
        spec: &WebInspectorCdpSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorCdpSession>> {
        self.ensure_available()?;
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        if let Some(tunnel) = tunnel.as_ref()
            && tunnel.info().udid != device.udid
        {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }
        self.inner
            .webinspector_cdp_count
            .fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MockWebInspectorCdpSession {
            backend: self.clone(),
            info: WebInspectorCdpSessionInfo {
                udid: device.udid,
                pages: vec![WebInspectorPageInfo {
                    key: "mock-page-1".into(),
                    page_id: 7,
                    application_id: "PID:42".into(),
                    bundle_id: "com.apple.mobilesafari".into(),
                    title: "Mock Page".into(),
                    url: "https://example.com/".into(),
                }],
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            attached_page: None,
            messages: VecDeque::new(),
            closed: false,
        }))
    }

    async fn start_webinspector_js_shell_session(
        &self,
        spec: &WebInspectorJsShellSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorJsShellSession>> {
        self.ensure_available()?;
        let device = self
            .resolve_device(&DeviceTarget {
                udid: spec.udid.clone(),
                ..DeviceTarget::default()
            })
            .await?;
        if let Some(tunnel) = tunnel.as_ref()
            && tunnel.info().udid != device.udid
        {
            return Err(CoreError::InvalidArgument(format!(
                "selected tunnel belongs to {}, not {}",
                tunnel.info().udid,
                device.udid
            )));
        }
        self.inner
            .webinspector_js_shell_count
            .fetch_add(1, Ordering::SeqCst);
        let bundle_id = spec.bundle_id.clone().unwrap_or_else(|| {
            if spec.open_safari {
                "com.apple.mobilesafari".into()
            } else {
                "com.example.mock".into()
            }
        });
        Ok(Box::new(MockWebInspectorJsShellSession {
            backend: self.clone(),
            info: WebInspectorJsShellSessionInfo {
                udid: device.udid,
                page: WebInspectorPageInfo {
                    key: "mock-page-1".into(),
                    page_id: 7,
                    application_id: "PID:42".into(),
                    bundle_id,
                    title: "Mock Page".into(),
                    url: spec
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://example.com/".into()),
                },
                service_transport: if tunnel.is_some() {
                    ServiceTransport::SharedRsd
                } else {
                    ServiceTransport::Mock
                },
            },
            closed: false,
        }))
    }

    async fn execute(&self, invocation: &CommandInvocation) -> CoreResult<CommandResult> {
        self.ensure_available()?;
        let command = invocation.argv.first().map(String::as_str).unwrap_or("");
        let mut warnings = Vec::new();
        let mut text = None;
        let value = match command {
            "activate" => {
                if invocation.argv.len() != 1 {
                    return Err(CoreError::InvalidArgument(
                        "activate accepts no command-specific arguments".into(),
                    ));
                }
                json!({
                    "status": "already_activated",
                    "activationState": "Activated",
                    "verified": true,
                    "udid": invocation.udid.as_deref().unwrap_or("mock-phone"),
                    "mock": true,
                })
            }
            "erase" => {
                let udid = validate_erase_invocation(invocation)?;
                json!({
                    "status": "erase_requested",
                    "preserveDataPlan": true,
                    "response": "acknowledged",
                    "udid": udid,
                    "mock": true,
                })
            }
            "set-wallpaper" => {
                let request = parse_set_wallpaper_invocation(invocation)?;
                json!({
                    "status": "set",
                    "path": request.image_path,
                    "bytes": 0,
                    "screen": request.screen.as_str(),
                    "where": request.screen.where_value(),
                    "supervised": true,
                    "mock": true,
                })
            }
            "pair" => {
                let (supervised, host_name) = match parse_pair_invocation(invocation)? {
                    PairInvocation::Standard { host_name } => (false, host_name),
                    PairInvocation::Supervised { .. } => (true, None),
                };
                json!({
                    "udid": invocation.udid.as_deref().unwrap_or("mock-phone"),
                    "hostID": "00000000-0000-4000-8000-000000000001",
                    "systemBUID": "00000000-0000-4000-8000-000000000002",
                    "hostName": host_name,
                    "saved": true,
                    "supervised": supervised,
                    "mock": true,
                })
            }
            "info" => match invocation.argv.get(1).map(String::as_str) {
                None | Some("lockdown") => json!({
                    "DeviceName": "Mock iPhone",
                    "ProductType": "iPhoneMock,1",
                    "ProductVersion": "18.0",
                    "UniqueDeviceID": invocation.udid.as_deref().unwrap_or("mock-phone"),
                }),
                Some("display") => {
                    return Err(CoreError::DeviceUnavailable(
                        "info display requires a running server-owned userspace tunnel; run tunnel start --userspace"
                            .into(),
                    ));
                }
                Some(action) => {
                    return Err(CoreError::InvalidArgument(format!(
                        "info accepts display or lockdown, got {action:?}"
                    )));
                }
            },
            "apps" => json!({}),
            "debug" => {
                let target = DeviceTarget {
                    udid: invocation.udid.clone(),
                    ..DeviceTarget::default()
                };
                self.resolve_device(&target).await?;
                serde_json::to_value(crate::debug::mock_debug_app_info(invocation)?).map_err(
                    |error| CoreError::Backend(format!("serialize debug app info: {error}")),
                )?
            }
            "diskspace" => json!({
                "Model": "MockFS",
                "BlockSize": 4096,
                "FreeBytes": 3_000_000_000_u64,
                "UsedBytes": 1_000_000_000_u64,
                "TotalBytes": 4_000_000_000_u64,
            }),
            "file" => mock_file(invocation, None).await?,
            "crash" => match invocation.argv.get(1).map(String::as_str) {
                Some("ls") => json!({ "files": [], "length": 0 }),
                Some("cp") => json!({ "files": [], "length": 0 }),
                Some("rm") => json!({ "removed": [], "length": 0 }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock crash action {action:?}"
                    )));
                }
            },
            "profile" => match invocation.argv.get(1).map(String::as_str) {
                Some("list") => json!([]),
                Some("add") => {
                    let silent = mock_option_value(&invocation.argv, "--p12file").is_some();
                    if !silent {
                        warnings.push(
                                "profile was submitted with InstallProfile; the device may require manual approval"
                                    .into(),
                            );
                    }
                    json!({ "status": "installed", "silent": silent, "mock": true })
                }
                Some("remove") => json!({ "status": "removed", "mock": true }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock profile action {action:?}"
                    )));
                }
            },
            "mdm" => {
                let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
                    CoreError::InvalidArgument(
                        "mdm requires security-info, fetch-unlock-token, clear-passcode or clear-screen-time-password"
                            .into(),
                    )
                })?;
                if mock_option_value(&invocation.argv, "--p12file").is_none() {
                    return Err(CoreError::InvalidArgument("mdm requires --p12file".into()));
                }
                match action {
                    "security-info" => json!({
                        "securityInfo": {
                            "PasscodePresent": false,
                            "PasscodeCompliant": true,
                            "PasscodeCompliantWithProfiles": true,
                            "ManagementStatus": "supervised",
                        },
                        "serviceTransport": "mock",
                    }),
                    "fetch-unlock-token" => json!({
                        "status": "fetched",
                        "path": mock_option_value(&invocation.argv, "--output")
                            .unwrap_or_else(|| "mock-unlock-token".into()),
                        "bytes": 32,
                        "mock": true,
                    }),
                    "clear-passcode" => json!({ "status": "cleared", "serviceTransport": "mock" }),
                    "clear-screen-time-password" => {
                        json!({ "status": "cleared", "serviceTransport": "mock" })
                    }
                    other => {
                        return Err(CoreError::InvalidArgument(format!(
                            "unknown mock mdm action {other:?}"
                        )));
                    }
                }
            }
            "wifi" => {
                let ssid = mock_option_value(&invocation.argv, "--ssid").ok_or_else(|| {
                    CoreError::InvalidArgument("wifi requires a non-empty --ssid".into())
                })?;
                if invocation.argv.iter().any(|value| value == "--remove") {
                    warnings.push(
                        "device is not supervised; Wi-Fi profile removal may require manual confirmation"
                            .into(),
                    );
                    json!({ "status": "removed", "ssid": ssid, "supervised": false, "mock": true })
                } else {
                    let _password =
                        mock_option_value(&invocation.argv, "--password").ok_or_else(|| {
                            CoreError::InvalidArgument(
                                "wifi requires a non-empty --password".into(),
                            )
                        })?;
                    let encryption_type = mock_option_value(&invocation.argv, "--enc-type")
                        .unwrap_or_else(|| "WPA".into());
                    warnings.push(
                        "device is not supervised; approve the Wi-Fi profile in Settings before it takes effect"
                            .into(),
                    );
                    json!({
                        "status": "installed",
                        "ssid": ssid,
                        "encryptionType": encryption_type,
                        "supervised": false,
                        "mock": true,
                    })
                }
            }
            "httpproxy" => {
                if invocation.argv.get(1).map(String::as_str) == Some("remove") {
                    json!({ "status": "removed", "mock": true })
                } else {
                    let host = invocation
                        .argv
                        .get(1)
                        .filter(|value| !value.starts_with('-'))
                        .ok_or_else(|| {
                            CoreError::InvalidArgument("httpproxy requires host".into())
                        })?;
                    let port = invocation
                        .argv
                        .get(2)
                        .filter(|value| !value.starts_with('-'))
                        .ok_or_else(|| {
                            CoreError::InvalidArgument("httpproxy requires port".into())
                        })?
                        .parse::<u16>()
                        .map_err(|_| {
                            CoreError::InvalidArgument("httpproxy port must be 1..65535".into())
                        })?;
                    if port == 0 || mock_option_value(&invocation.argv, "--p12file").is_none() {
                        return Err(CoreError::InvalidArgument(
                            "httpproxy requires port 1..65535 and --p12file".into(),
                        ));
                    }
                    json!({
                        "status": "installed",
                        "host": host,
                        "port": port,
                        "authenticated": invocation.argv.get(3).is_some_and(|value| !value.starts_with('-')),
                        "mock": true,
                    })
                }
            }
            "prepare" => match invocation.argv.get(1).map(String::as_str) {
                Some("cloudconfig") => json!({
                    "Status": "Acknowledged",
                    "IsSupervised": false,
                    "CloudConfigurationIsComplete": false,
                    "mock": true,
                }),
                Some("printskip") => json!(SETUP_SKIP_KEYS),
                Some("create-cert") => {
                    return Err(CoreError::NotImplemented(
                        "mock prepare create-cert is available through the real host backend"
                            .into(),
                    ));
                }
                Some(action) if !action.starts_with('-') => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock prepare action {action:?}"
                    )));
                }
                _ => json!({
                    "status": "prepared",
                    "supervised": mock_option_value(&invocation.argv, "--certfile").is_some(),
                    "mock": true,
                }),
            },
            "diagnostics" => {
                if invocation.argv.len() != 2
                    || invocation.argv.get(1).map(String::as_str) != Some("list")
                {
                    return Err(CoreError::InvalidArgument(
                        "diagnostics requires exactly the list subcommand".into(),
                    ));
                }
                json!({
                    "Diagnostics": {
                        "GasGauge": {
                            "CycleCount": 0,
                            "DesignCapacity": 0,
                            "FullChargeCapacity": 0,
                            "Status": "Success",
                        },
                        "HDMI": { "Connection": "Inactive", "Status": "Success" },
                        "IORegistry": {
                            "AbsoluteCapacity": 0,
                            "AppleRawCurrentCapacity": 0,
                            "AppleRawMaxCapacity": 0,
                            "AtCriticalLevel": false,
                            "AtWarnLevel": false,
                            "CurrentCapacity": 0,
                            "CycleCount": 0,
                            "DesignCapacity": 0,
                            "InstantAmperage": 0,
                            "IsCharging": false,
                            "NominalChargeCapacity": 0,
                            "Temperature": 0,
                            "Voltage": 0,
                        },
                        "NAND": { "Status": "Success" },
                        "WiFi": { "Active": "", "Status": "Success" },
                    },
                    "Status": "Success",
                })
            }
            "batterycheck" | "batteryregistry" => json!({
                "BatteryCurrentCapacity": 87,
                "BatteryIsCharging": false,
                "ExternalChargeCapable": true,
            }),
            "mobilegestalt" => {
                let mobilegestalt = plist::Dictionary::from_iter([
                    ("MockGestalt".to_owned(), plist::Value::Boolean(true)),
                    (
                        "Status".to_owned(),
                        plist::Value::String("Success".to_owned()),
                    ),
                ]);
                let diagnostics = plist::Dictionary::from_iter([(
                    "MobileGestalt".to_owned(),
                    plist::Value::Dictionary(mobilegestalt),
                )]);
                let response = crate::mobilegestalt_envelope(diagnostics);
                if invocation.argv.iter().any(|value| value == "--plist") {
                    return Ok(CommandResult {
                        exit_code: 0,
                        value: None,
                        text: Some(crate::plist_dictionary_xml(&response)?),
                        warnings,
                    });
                }
                serde_json::to_value(response)
                    .map_err(|error| CoreError::Backend(error.to_string()))?
            }
            "devicename" => json!({ "DeviceName": "Mock iPhone" }),
            "date" => {
                let formatted = "Thursday, 01-Jan-70 00:00:00 UTC".to_owned();
                text = Some(formatted.clone());
                json!({
                    "TimeIntervalSince1970": 0.0,
                    "formatedDate": formatted,
                })
            }
            "ip" => json!({
                "Mac": "02:00:00:00:00:01",
                "IPv4": "192.0.2.10",
                "IPv6": "2001:db8::10",
            }),
            "lang" => json!({
                "Language": mock_option_value(&invocation.argv, "--setlang")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "en".into()),
                "Locale": mock_option_value(&invocation.argv, "--setlocale")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "en_US".into()),
                "SupportedLocales": ["en_US", "zh_CN"],
                "SupportedLanguages": ["en", "zh-Hans"],
            }),
            "assistivetouch" | "voiceover" | "zoom" | "timeformat" => {
                mock_lockdown_toggle(invocation)?
            }
            "reboot" | "shutdown" | "resetlocation" | "setlocation" => {
                json!({ "status": "ok", "mock": true })
            }
            "devmode" => match invocation.argv.get(1).map(String::as_str) {
                Some("get") => json!({ "DeveloperModeEnabled": true }),
                Some("enable") => json!({ "status": "restarting", "mock": true }),
                Some("reveal") => json!({ "status": "revealed", "mock": true }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock devmode action {action:?}"
                    )));
                }
            },
            "image" => match invocation.argv.get(1).map(String::as_str) {
                Some("list") => json!([]),
                Some("mount") => json!({ "status": "mounted", "mock": true }),
                Some("unmount") => json!({ "status": "unmounted", "mock": true }),
                Some("auto") => json!({
                    "status": "mounted",
                    "source": "auto",
                    "buildVersion": "23F84",
                    "mock": true,
                }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock image action {action:?}"
                    )));
                }
            },
            "devicestate" => match invocation.argv.get(1).map(String::as_str) {
                Some("list") => json!([{
                    "Identifier": "SlowNetworkCondition",
                    "Profiles": [{
                        "Identifier": "SlowNetwork3GGood",
                        "Description": "Mock 3G network condition",
                    }],
                }]),
                Some("enable") => {
                    return Err(CoreError::NotImplemented(
                        "mock devicestate enable must be owned by the server task manager".into(),
                    ));
                }
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock devicestate action {action:?}"
                    )));
                }
            },
            "ps" => json!([{
                "pid": 42,
                "name": "MockApp",
                "realAppName": "MockApp",
                "isApplication": true,
                "startPageCount": 1,
            }]),
            "launch" => json!({
                "bundleID": invocation.argv.get(1),
                "pid": 42,
                "mock": true,
            }),
            "kill" => json!({ "status": "killed", "pid": 42, "mock": true }),
            "memlimitoff" => {
                json!({ "status": "disabled", "pid": 42, "mock": true })
            }
            "webinspector" => mock_webinspector(invocation)?,
            "ax" => match invocation.argv.get(1).map(String::as_str) {
                Some("audit") => json!({
                    "apiVersion": 21,
                    "auditTypes": [12, 100, 5000],
                    "issueCount": 1,
                    "issues": [{
                        "issueType": "testTypeContrast",
                        "label": "Mock accessibility element",
                        "elementRect": { "x": 10.0, "y": 20.0, "width": 120.0, "height": 44.0 },
                        "platformElementValue": "UGdBAQ==",
                    }],
                    "serviceTransport": "mock",
                    "mock": true,
                }),
                Some(action) => {
                    return Err(CoreError::NotImplemented(format!(
                        "mock ax action {action:?} is not implemented"
                    )));
                }
                None => {
                    return Err(CoreError::InvalidArgument(
                        "ax requires the audit subcommand".into(),
                    ));
                }
            },
            "resetax" => json!({
                "status": "reset",
                "serviceTransport": "mock",
                "mock": true,
            }),
            "ui" => mock_ui(invocation)?,
            "get-app-icon" => {
                let request = parse_app_icon_request(invocation)?;
                write_png_atomic(
                    &request.output_path,
                    MOCK_APP_ICON_PNG,
                    "mock SpringBoard icon",
                )
                .await?;
                json!({
                    "bundleID": request.bundle_identifier,
                    "path": request.output_path,
                    "bytes": MOCK_APP_ICON_PNG.len(),
                    "mock": true,
                })
            }
            "get-wallpaper" => json!({ "path": "wallpaper.png", "bytes": 0, "mock": true }),
            "get-icon-layout" => json!([[{
                "bundleIdentifier": "com.example.Mock",
                "displayName": "Mock",
            }]]),
            "set-icon-layout" => json!({ "status": "set", "mock": true }),
            "pasteboard" => match invocation.argv.get(1).map(String::as_str) {
                Some("get") => json!({
                    "pasteboard": "general",
                    "changeCount": 7,
                    "text": "mock pasteboard text",
                    "items": 1,
                    "imageUti": null,
                    "imageBytes": null,
                    "promisedItems": 0,
                    "serviceTransport": "mock",
                }),
                Some("set") => json!({
                    "status": "set",
                    "pasteboard": "general",
                    "serviceTransport": "mock",
                }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock pasteboard action {action:?}"
                    )));
                }
            },
            "fsync" => match invocation.argv.get(1).map(String::as_str) {
                Some("tree") => json!({
                    "path": "/",
                    "entries": [{ "path": "/", "type": "S_IFDIR", "size": 0 }],
                    "count": 1,
                }),
                Some("mkdir") => json!({ "status": "created", "mock": true }),
                Some("rm") => json!({ "status": "removed", "mock": true }),
                Some("pull") => json!({
                    "files": 1,
                    "directories": 0,
                    "bytes": 16,
                    "mock": true,
                }),
                Some("push") => json!({ "bytes": 16, "mock": true }),
                action => {
                    return Err(CoreError::InvalidArgument(format!(
                        "unknown mock fsync action {action:?}"
                    )));
                }
            },
            "sign" => {
                if invocation.argv.get(1).map(String::as_str) != Some("app")
                    || !invocation.argv.iter().any(|value| value == "--install")
                {
                    return Err(CoreError::NotImplemented(
                        "mock supports only sign app --install as a device operation".into(),
                    ));
                }
                json!({
                    "status": "complete",
                    "appPath": mock_option_value(&invocation.argv, "--output")
                        .unwrap_or_else(|| "mock-signed.app".into()),
                    "bundleID": mock_option_value(&invocation.argv, "--bundleid"),
                    "mock": true,
                })
            }
            "screenshot" => {
                if invocation.argv.iter().any(|value| value == "--stream")
                    || mock_option_value(&invocation.argv, "--port").is_some()
                {
                    return Err(CoreError::NotImplemented(
                        "mock screenshot streaming must be owned by the server task manager".into(),
                    ));
                }
                let output = mock_output_path(
                    invocation,
                    &mock_option_value(&invocation.argv, "--output")
                        .unwrap_or_else(|| "screenshot.png".into()),
                );
                let bytes = crate::springboard::MOCK_APP_ICON_PNG;
                mock_write_output(&output, bytes)?;
                json!({
                    "output": output,
                    "bytes": bytes.len(),
                    "mock": true,
                })
            }
            "uninstall" | "install" => json!({ "status": "complete", "mock": true }),
            _ => {
                return Err(CoreError::NotImplemented(format!(
                    "mock backend does not implement command {command:?}"
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

    async fn execute_with_tunnel(
        &self,
        invocation: &CommandInvocation,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<CommandResult> {
        if invocation.argv.first().map(String::as_str) == Some("info")
            && invocation.argv.get(1).map(String::as_str) == Some("display")
        {
            let tunnel = tunnel.ok_or_else(|| {
                CoreError::DeviceUnavailable(
                    "info display requires a running server-owned userspace tunnel; run tunnel start --userspace"
                        .into(),
                )
            })?;
            tunnel
                .ensure_rsd_service(RsdServiceKind::DeviceInfo)
                .await?;
            return Ok(CommandResult {
                exit_code: 0,
                value: Some(json!({
                    "backlightState": 2,
                    "current": { "displayId": 1 },
                    "displays": [{ "displayId": 1, "width": 828, "height": 1792 }],
                    "orientation": 1,
                })),
                text: None,
                warnings: vec!["reused server-owned shared RSD tunnel".into()],
            });
        }
        if invocation.argv.first().map(String::as_str) == Some("file") {
            let value = mock_file(invocation, tunnel.as_deref()).await?;
            let mut warnings = Vec::new();
            if tunnel.is_some() && crate::file::has_domain_selector(&invocation.argv) {
                warnings.push("reused server-owned shared RSD tunnel".into());
            }
            return Ok(CommandResult {
                exit_code: 0,
                value: Some(value),
                text: None,
                warnings,
            });
        }
        let mut result = self.execute(invocation).await?;
        if tunnel.is_some() && mock_command_uses_shared_rsd(invocation) {
            result
                .warnings
                .push("reused server-owned shared RSD tunnel".into());
        }
        Ok(result)
    }
}

fn mock_xctest_case(target: &rust_ios_protocol::XCTestTargetSpec) -> (String, String) {
    let Some(identifier) = target.tests_to_run.first() else {
        return ("MockTests".into(), "testExample".into());
    };
    let slash = identifier.find('/');
    let dot = identifier.find('.');
    let identifier = match (dot, slash) {
        (Some(dot), Some(slash)) if dot < slash => &identifier[dot + 1..],
        (Some(dot), None) => &identifier[dot + 1..],
        _ => identifier,
    };
    let (class, method) = identifier
        .split_once('/')
        .map_or((identifier, "testExample"), |(class, method)| {
            (class, method)
        });
    (class.into(), method.into())
}

fn mock_service_transport(
    kind: &TaskKind,
    tunnel: Option<&Arc<dyn UserSpaceTunnel>>,
) -> ServiceTransport {
    if tunnel.is_some()
        && matches!(
            kind,
            TaskKind::Notifications | TaskKind::Sysmontap | TaskKind::ScreenshotStream
        )
    {
        ServiceTransport::SharedRsd
    } else {
        ServiceTransport::Mock
    }
}

fn mock_command_uses_shared_rsd(invocation: &CommandInvocation) -> bool {
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

async fn mock_file(
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let selection = crate::file::parse_selection(&invocation.argv)?;
    if selection
        .as_ref()
        .is_some_and(crate::file::FileServiceSelection::requires_tunnel)
        && tunnel.is_none()
    {
        return Err(CoreError::DeviceUnavailable(
            "FileService domain requires a running server-owned userspace tunnel; run tunnel start --userspace"
                .into(),
        ));
    }
    if selection.is_some()
        && let Some(tunnel) = tunnel
    {
        tunnel
            .ensure_rsd_service(RsdServiceKind::FileServiceControl)
            .await?;
        tunnel
            .ensure_rsd_service(RsdServiceKind::FileServiceData)
            .await?;
    }
    let action = invocation
        .argv
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument("file requires ls, pull or push".into()))?;
    let domain = selection
        .as_ref()
        .map(|selection| selection.domain.label())
        .unwrap_or("afc");
    let identifier = selection
        .as_ref()
        .and_then(|selection| (!selection.identifier.is_empty()).then_some(&selection.identifier));
    let transport = if selection.is_some() && tunnel.is_some() {
        "shared_rsd"
    } else {
        "mock"
    };
    match action {
        "ls" => {
            let path = mock_option_value(&invocation.argv, "--path")
                .unwrap_or_else(|| if selection.is_some() { "." } else { "/" }.into());
            let files = vec!["mock-file.txt"];
            Ok(json!({
                "path": path,
                "files": files,
                "count": files.len(),
                "domain": domain,
                "identifier": identifier,
                "serviceTransport": transport,
                "mock": true,
            }))
        }
        "pull" => {
            let remote = mock_option_value(&invocation.argv, "--remote")
                .ok_or_else(|| CoreError::InvalidArgument("file pull requires --remote".into()))?;
            let local = mock_option_value(&invocation.argv, "--local")
                .ok_or_else(|| CoreError::InvalidArgument("file pull requires --local".into()))?;
            let output = mock_output_path(invocation, &local);
            let bytes = format!("rust-ios mock FileService {domain} {remote}\n");
            mock_write_output(&output, bytes.as_bytes())?;
            Ok(json!({
                "remote": remote,
                "local": output,
                "size": bytes.len(),
                "bytes": bytes.len(),
                "domain": domain,
                "identifier": identifier,
                "serviceTransport": transport,
                "mock": true,
            }))
        }
        "push" => {
            let local = mock_option_value(&invocation.argv, "--local")
                .ok_or_else(|| CoreError::InvalidArgument("file push requires --local".into()))?;
            let remote = mock_option_value(&invocation.argv, "--remote")
                .ok_or_else(|| CoreError::InvalidArgument("file push requires --remote".into()))?;
            let input = mock_output_path(invocation, &local);
            let metadata = std::fs::metadata(&input)?;
            if !metadata.is_file() {
                return Err(CoreError::InvalidArgument(format!(
                    "file push source {} is not a file",
                    input.display()
                )));
            }
            Ok(json!({
                "remote": remote,
                "local": input,
                "size": metadata.len(),
                "bytes": metadata.len(),
                "domain": domain,
                "identifier": identifier,
                "serviceTransport": transport,
                "mock": true,
            }))
        }
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown file action {action:?}"
        ))),
    }
}

fn mock_webinspector(invocation: &CommandInvocation) -> CoreResult<Value> {
    match invocation.argv.get(1).map(String::as_str) {
        Some("list") => Ok(json!([{
            "application": {
                "id": "PID:42",
                "bundleId": "com.apple.mobilesafari",
                "pid": 42,
                "name": "Safari",
                "automationAvailability": "WIRAutomationAvailabilityAvailable",
                "active": true,
                "proxy": false,
                "ready": true,
            },
            "page": {
                "id": 7,
                "key": "mock-page-1",
                "type": "WIRTypeWebPage",
                "url": "https://example.com/",
                "title": "Mock Page",
            }
        }])),
        Some("eval") => {
            let page_key = invocation.argv.get(2).ok_or_else(|| {
                CoreError::InvalidArgument(
                    "webinspector eval requires pageID and expression".into(),
                )
            })?;
            let expression = invocation.argv.get(3).ok_or_else(|| {
                CoreError::InvalidArgument(
                    "webinspector eval requires pageID and expression".into(),
                )
            })?;
            if page_key != "mock-page-1" {
                return Err(CoreError::DeviceUnavailable(format!(
                    "no inspectable WebInspector page with key {page_key:?} was found"
                )));
            }
            Ok(match expression.as_str() {
                "2+2" => json!(4),
                "document.title" => json!("Mock Page"),
                _ => json!({ "expression": expression, "mock": true }),
            })
        }
        Some("launch") => {
            let url = invocation
                .argv
                .get(2)
                .filter(|value| !value.starts_with('-'))
                .ok_or_else(|| {
                    CoreError::InvalidArgument("webinspector launch requires a URL".into())
                })?;
            Ok(json!({
                "bundleID": mock_option_value(&invocation.argv, "--bundle-id")
                    .unwrap_or_else(|| "com.apple.mobilesafari".into()),
                "url": url,
                "title": "Mock Page",
                "mock": true,
            }))
        }
        Some("js-shell" | "cdp") => Err(CoreError::NotImplemented(
            "mock WebInspector persistent sessions must be owned by the server task manager".into(),
        )),
        action => Err(CoreError::InvalidArgument(format!(
            "unknown mock webinspector action {action:?}"
        ))),
    }
}

fn mock_ui(invocation: &CommandInvocation) -> CoreResult<Value> {
    let action = invocation
        .argv
        .get(1)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument("ui requires a subcommand".into()))?;
    let driver =
        mock_option_value(&invocation.argv, "--driver").unwrap_or_else(|| "devicekit".into());
    match action {
        "download" => Err(CoreError::NotImplemented(
            "mock ui download is handled by the shared host command path".into(),
        )),
        "install" => {
            let target = invocation
                .argv
                .get(2)
                .filter(|value| matches!(value.as_str(), "wda" | "devicekit"))
                .ok_or_else(|| {
                    CoreError::InvalidArgument("ui install requires wda or devicekit".into())
                })?;
            if mock_option_value(&invocation.argv, "--p12file").is_none()
                || mock_option_value(&invocation.argv, "--profile").is_none()
            {
                return Err(CoreError::InvalidArgument(
                    "ui install requires --p12file and --profile".into(),
                ));
            }
            Ok(json!({
                "status": "complete",
                "target": target,
                "appPath": mock_option_value(&invocation.argv, "--output")
                    .unwrap_or_else(|| format!("mock-{target}-signed.app")),
                "bundleID": mock_option_value(&invocation.argv, "--bundleid"),
                "mock": true,
            }))
        }
        "run" => Err(CoreError::NotImplemented(
            "mock ui run must be owned by the server task manager".into(),
        )),
        "stream" => Err(CoreError::NotImplemented(
            "mock ui stream must be owned by the server task manager".into(),
        )),
        "status" => Ok(json!({
            "driver": driver,
            "status": "ok",
            "ready": true,
            "mock": true,
        })),
        "api" | "raw" => Ok(json!({
            "driver": driver,
            "method": mock_option_value(&invocation.argv, "--rpc-method")
                .or_else(|| mock_option_value(&invocation.argv, "--method")),
            "path": mock_option_value(&invocation.argv, "--http-path"),
            "mock": true,
        })),
        "tap" | "swipe" | "longpress" | "type" | "button" => Ok(json!({
            "driver": driver,
            "action": action,
            "status": "ok",
            "mock": true,
        })),
        "screenshot" => {
            let output = mock_option_value(&invocation.argv, "--output").ok_or_else(|| {
                CoreError::NotImplemented(
                    "ui screenshot without --output requires binary stdout transport".into(),
                )
            })?;
            if output == "-" {
                return Err(CoreError::NotImplemented(
                    "ui screenshot --output=- requires binary stdout transport".into(),
                ));
            }
            let path = mock_output_path(invocation, &output);
            mock_write_output(&path, b"\x89PNG\r\n\x1a\n")?;
            Ok(json!({
                "driver": driver,
                "output": path,
                "bytes": 8,
                "mock": true,
            }))
        }
        "source" => {
            if let Some(output) = mock_option_value(&invocation.argv, "--output")
                && output != "-"
            {
                let path = mock_output_path(invocation, &output);
                mock_write_output(&path, b"<MockUI />")?;
                return Ok(json!({
                    "driver": driver,
                    "output": path,
                    "bytes": 10,
                    "mock": true,
                }));
            }
            Ok(json!({
                "driver": driver,
                "value": "<MockUI />",
                "mock": true,
            }))
        }
        "size" => Ok(json!({
            "driver": driver,
            "value": { "width": 390, "height": 844 },
            "mock": true,
        })),
        "orientation" => Ok(json!({
            "driver": driver,
            "value": "PORTRAIT",
            "mock": true,
        })),
        "app" => Ok(json!({
            "driver": driver,
            "status": "ok",
            "mock": true,
        })),
        _ => Err(CoreError::InvalidArgument(format!(
            "unknown mock ui action {action:?}"
        ))),
    }
}

fn mock_output_path(invocation: &CommandInvocation, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else if let Some(cwd) = &invocation.cwd {
        Path::new(cwd).join(path)
    } else {
        path.to_owned()
    }
}

fn mock_write_output(path: &Path, bytes: &[u8]) -> CoreResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

fn mock_option_value(arguments: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for (index, argument) in arguments.iter().enumerate() {
        if let Some(value) = argument.strip_prefix(&prefix) {
            return Some(value.into());
        }
        if argument == name {
            return arguments.get(index + 1).cloned();
        }
    }
    None
}

fn mock_lockdown_toggle(invocation: &CommandInvocation) -> CoreResult<Value> {
    let command = invocation.argv[0].as_str();
    let action =
        invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
            CoreError::InvalidArgument(format!("{command} requires an operation"))
        })?;
    let (output_key, time_format) = match command {
        "assistivetouch" => ("AssistiveTouchEnabled", false),
        "voiceover" => ("VoiceOverTouchEnabled", false),
        "zoom" => ("ZoomTouchEnabled", false),
        "timeformat" => ("TimeFormat", true),
        _ => unreachable!("match arm only calls known mock toggle commands"),
    };
    let desired = match (time_format, action) {
        (_, "get") => false,
        (_, "toggle") => true,
        (false, "enable") | (true, "24h") => true,
        (false, "disable") | (true, "12h") => false,
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
    let mut result = serde_json::Map::new();
    result.insert(
        output_key.into(),
        if time_format {
            Value::String(if desired { "24h" } else { "12h" }.into())
        } else {
            Value::Bool(desired)
        },
    );
    if action != "get" {
        result.insert("status".into(), Value::String("set".into()));
        result.insert(
            "changed".into(),
            Value::Bool(invocation.argv.iter().any(|value| value == "--force") || desired),
        );
    }
    Ok(Value::Object(result))
}

#[derive(Debug)]
struct MockDeviceCondition {
    backend: MockDeviceBackend,
    info: DeviceConditionInfo,
    closed: AtomicBool,
}

#[derive(Debug)]
struct MockLocationSimulation {
    backend: MockDeviceBackend,
    info: LocationSimulationInfo,
    closed: bool,
}

#[async_trait]
impl DeviceLocationSimulation for MockLocationSimulation {
    fn info(&self) -> &LocationSimulationInfo {
        &self.info
    }

    async fn set_location(&mut self, latitude: f64, longitude: f64) -> CoreResult<()> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock location simulation is closed".into(),
            ));
        }
        self.backend
            .inner
            .location_points
            .write()
            .await
            .push(LocationSimulationPoint {
                latitude,
                longitude,
                delay_before_ms: 0,
                source_time: None,
            });
        Ok(())
    }

    async fn clear(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.backend
                .inner
                .location_clear_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.closed = true;
        Ok(())
    }
}

#[async_trait]
impl DeviceCondition for MockDeviceCondition {
    fn info(&self) -> &DeviceConditionInfo {
        &self.info
    }

    async fn close(&self) -> CoreResult<()> {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.backend
                .inner
                .condition_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct MockWdaSession {
    backend: MockDeviceBackend,
    info: WdaSessionInfo,
    events: VecDeque<XCTestEvent>,
    closed: bool,
}

#[derive(Debug)]
struct MockUiRunnerSession {
    backend: MockDeviceBackend,
    info: UiRunnerSessionInfo,
    events: VecDeque<XCTestEvent>,
    closed: bool,
}

#[derive(Debug)]
struct MockXCTestSession {
    info: XCTestSessionInfo,
    updates: VecDeque<XCTestSessionUpdate>,
    closed: bool,
}

#[derive(Debug)]
struct MockAppProcessSession {
    backend: MockDeviceBackend,
    info: AppProcessSessionInfo,
    chunks: VecDeque<Vec<u8>>,
    closed: bool,
}

#[derive(Debug)]
struct MockWebInspectorCdpSession {
    backend: MockDeviceBackend,
    info: WebInspectorCdpSessionInfo,
    attached_page: Option<String>,
    messages: VecDeque<Value>,
    closed: bool,
}

#[derive(Debug)]
struct MockWebInspectorJsShellSession {
    backend: MockDeviceBackend,
    info: WebInspectorJsShellSessionInfo,
    closed: bool,
}

#[async_trait]
impl WebInspectorCdpSession for MockWebInspectorCdpSession {
    fn info(&self) -> &WebInspectorCdpSessionInfo {
        &self.info
    }

    async fn refresh_pages(&mut self) -> CoreResult<Vec<WebInspectorPageInfo>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WebInspector CDP session is closed".into(),
            ));
        }
        Ok(self.info.pages.clone())
    }

    async fn attach_page(&mut self, page_key: &str) -> CoreResult<()> {
        if !self.info.pages.iter().any(|page| page.key == page_key) {
            return Err(CoreError::DeviceUnavailable(format!(
                "no inspectable WebInspector page with key {page_key:?} was found"
            )));
        }
        self.attached_page = Some(page_key.to_owned());
        Ok(())
    }

    async fn send_message(&mut self, message: Value) -> CoreResult<()> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WebInspector CDP session is closed".into(),
            ));
        }
        if self.attached_page.is_none() {
            return Err(CoreError::InvalidArgument(
                "CDP page session has not been attached".into(),
            ));
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        self.messages.push_back(json!({
            "id": id,
            "result": {
                "mock": true,
                "method": method,
            }
        }));
        Ok(())
    }

    async fn next_message(&mut self, timeout: Duration) -> CoreResult<Option<Value>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WebInspector CDP session is closed".into(),
            ));
        }
        if let Some(message) = self.messages.pop_front() {
            return Ok(Some(message));
        }
        tokio::time::sleep(timeout.min(Duration::from_millis(10))).await;
        Ok(None)
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .webinspector_cdp_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait]
impl WebInspectorJsShellSession for MockWebInspectorJsShellSession {
    fn info(&self) -> &WebInspectorJsShellSessionInfo {
        &self.info
    }

    async fn evaluate(&mut self, expression: &str) -> CoreResult<Value> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WebInspector JS shell session is closed".into(),
            ));
        }
        if expression.trim().is_empty() {
            return Err(CoreError::InvalidArgument(
                "WebInspector JS shell expression cannot be empty".into(),
            ));
        }
        if matches!(expression.replace(' ', "").as_str(), "2+2") {
            return Ok(json!(4));
        }
        if expression.trim_start().starts_with("throw ") {
            return Err(CoreError::Backend("mock JavaScript exception".into()));
        }
        Ok(json!({ "mock": true, "expression": expression }))
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .webinspector_js_shell_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait]
impl AppProcessSession for MockAppProcessSession {
    fn info(&self) -> &AppProcessSessionInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Option<Vec<u8>>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock app process session is closed".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(self.chunks.pop_front())
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .app_process_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait]
impl XCTestSession for MockXCTestSession {
    fn info(&self) -> &XCTestSessionInfo {
        &self.info
    }

    async fn next_update(&mut self) -> CoreResult<XCTestSessionUpdate> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock XCTest session is closed".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.updates
            .pop_front()
            .ok_or_else(|| CoreError::Backend("mock XCTest session ended without a summary".into()))
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.closed = true;
        Ok(())
    }
}

#[async_trait]
impl WdaSession for MockWdaSession {
    fn info(&self) -> &WdaSessionInfo {
        &self.info
    }

    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WDA session is closed".into(),
            ));
        }
        Ok(self.events.pop_front())
    }

    async fn status(&mut self) -> CoreResult<Value> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock WDA session is closed".into(),
            ));
        }
        Ok(self.info.status.clone())
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .wda_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[async_trait]
impl UiRunnerSession for MockUiRunnerSession {
    fn info(&self) -> &UiRunnerSessionInfo {
        &self.info
    }

    async fn ensure_running(&mut self) -> CoreResult<()> {
        if self.closed {
            Err(CoreError::DeviceUnavailable(
                "mock UI runner session is closed".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock UI runner session is closed".into(),
            ));
        }
        Ok(self.events.pop_front())
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .ui_runner_active_count
                .fetch_sub(1, Ordering::SeqCst);
            self.backend
                .inner
                .ui_runner_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct MockDeviceStream {
    info: DeviceStreamInfo,
    kind: TaskKind,
    format: StreamFormat,
    os_trace: OsTraceOptions,
    udid: String,
    sequence: u32,
    closed: bool,
}

#[async_trait]
impl DeviceStream for MockDeviceStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable("mock stream is closed".into()));
        }
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.sequence = self.sequence.saturating_add(1);
            match self.kind {
                TaskKind::OsTrace => {
                    let entry = mock_os_trace_entry(&self.udid, self.sequence);
                    if !mock_os_trace_entry_matches(&entry, &self.os_trace) {
                        continue;
                    }
                    if self.format == StreamFormat::JsonLines {
                        let label = entry
                            .subsystem
                            .map(|subsystem| json!({ "subsystem": subsystem, "category": "mock" }));
                        return Ok(format!(
                            "{}\n",
                            json!({
                                "pid": 42,
                                "timestamp": "2026-01-01T00:00:00",
                                "level": mock_os_trace_level_name(entry.level),
                                "image_name": "MockApp",
                                "filename": "/mock/MockApp",
                                "message": entry.message,
                                "label": label,
                            })
                        )
                        .into_bytes());
                    }
                    return Ok(format!("{}\n", entry.message).into_bytes());
                }
                TaskKind::Syslog | TaskKind::Notifications | TaskKind::Sysmontap => {
                    let label = match self.kind {
                        TaskKind::Syslog => "syslog",
                        TaskKind::Notifications => "notifications",
                        TaskKind::Sysmontap => "sysmontap",
                        _ => unreachable!(),
                    };
                    let message = format!("mock {label} {} {}", self.udid, self.sequence);
                    if self.format == StreamFormat::JsonLines {
                        return Ok(format!("{}\n", json!({ "message": message })).into_bytes());
                    } else {
                        return Ok(format!("{message}\n").into_bytes());
                    }
                }
                TaskKind::Pcap => return Ok(pcap_record(self.sequence)),
                TaskKind::ScreenshotStream => {
                    return Ok(vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x02, 0xff, 0xd9]);
                }
                _ => return Err(CoreError::NotImplemented("mock stream kind".into())),
            }
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.closed = true;
        Ok(())
    }
}

struct MockOsTraceEntry {
    level: OsTraceLevel,
    subsystem: Option<&'static str>,
    message: String,
}

fn mock_os_trace_entry(udid: &str, sequence: u32) -> MockOsTraceEntry {
    match sequence % 3 {
        1 => MockOsTraceEntry {
            level: OsTraceLevel::Info,
            subsystem: Some("com.example.network"),
            message: format!("mock ostrace {udid} {sequence} connected"),
        },
        2 => MockOsTraceEntry {
            level: OsTraceLevel::Error,
            subsystem: Some("com.example.storage"),
            message: format!("mock ostrace {udid} {sequence} connection timeout"),
        },
        _ => MockOsTraceEntry {
            level: OsTraceLevel::Fault,
            subsystem: None,
            message: format!("mock ostrace {udid} {sequence} ignored fatal timeout"),
        },
    }
}

fn mock_os_trace_entry_matches(entry: &MockOsTraceEntry, options: &OsTraceOptions) -> bool {
    (options.levels.is_empty() || options.levels.contains(&entry.level))
        && options
            .subsystem_contains
            .as_ref()
            .is_none_or(|filter| entry.subsystem.is_some_and(|value| value.contains(filter)))
        && options
            .message_contains
            .as_ref()
            .is_none_or(|filter| entry.message.contains(filter))
        && options
            .message_excludes
            .as_ref()
            .is_none_or(|filter| !entry.message.contains(filter))
}

fn mock_os_trace_level_name(level: OsTraceLevel) -> &'static str {
    match level {
        OsTraceLevel::Default => "Notice",
        OsTraceLevel::Info => "Info",
        OsTraceLevel::Debug => "Debug",
        OsTraceLevel::Error => "Error",
        OsTraceLevel::Fault => "Fault",
    }
}

#[derive(Debug)]
struct MockAccessibilityInspectorStream {
    info: DeviceStreamInfo,
    udid: String,
    font_size: Option<f64>,
    sequence: u32,
    queued_start: bool,
    closed: bool,
}

#[async_trait]
impl DeviceStream for MockAccessibilityInspectorStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock Accessibility inspector is closed".into(),
            ));
        }
        let value = if self.queued_start {
            self.queued_start = false;
            json!({
                "event": "inspector_started",
                "apiVersion": 26,
                "capabilities": ["deviceInspectorMoveWithOptions:"],
                "supportedAuditTypes": [1, 2, 3],
                "fontSize": self.font_size,
                "serviceTransport": "mock",
            })
        } else {
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.sequence = self.sequence.saturating_add(1);
            json!({
                "event": "notification",
                "selector": "hostInspectorCurrentElementChanged:",
                "focus": {
                    "platform_identifier": format!("MOCK-{}", self.sequence),
                    "estimated_uid": format!("mock-{}", self.sequence),
                    "caption": format!("Mock element {}", self.sequence),
                    "spoken_description": format!("Mock Accessibility element {}", self.sequence),
                },
                "data": {
                    "udid": self.udid,
                    "sequence": self.sequence,
                },
            })
        };
        let mut bytes = serde_json::to_vec(&value).map_err(|error| {
            CoreError::Backend(format!("serialize mock Accessibility event: {error}"))
        })?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.closed = true;
        Ok(())
    }
}

fn pcap_header() -> Vec<u8> {
    vec![
        0xd4, 0xc3, 0xb2, 0xa1, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xff, 0xff, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    ]
}

fn pcap_record(sequence: u32) -> Vec<u8> {
    let mut packet = vec![
        0xbe, 0xef, 0xbe, 0xef, 0xbe, 0xef, 0xbe, 0xef, 0xbe, 0xef, 0xbe, 0xef,
    ];
    packet.extend_from_slice(&[0x08, 0x00, 0x45, 0x00]);
    packet.extend_from_slice(&sequence.to_le_bytes());
    let length = packet.len() as u32;
    let mut record = Vec::with_capacity(16 + packet.len());
    record.extend_from_slice(&sequence.to_le_bytes());
    record.extend_from_slice(&0_u32.to_le_bytes());
    record.extend_from_slice(&length.to_le_bytes());
    record.extend_from_slice(&length.to_le_bytes());
    record.extend_from_slice(&packet);
    record
}

#[derive(Debug)]
struct MockTunnel {
    backend: MockDeviceBackend,
    info: UserSpaceTunnelInfo,
    closed: AtomicBool,
}

#[async_trait]
impl UserSpaceTunnel for MockTunnel {
    fn info(&self) -> &UserSpaceTunnelInfo {
        &self.info
    }

    async fn connect(&self, _port: u16) -> CoreResult<BoxedIo> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable("mock tunnel is closed".into()));
        }
        self.backend.echo_stream().await
    }

    async fn rsd_services(&self) -> CoreResult<BTreeMap<String, RsdServiceInfo>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable("mock tunnel is closed".into()));
        }
        let mut services = BTreeMap::from([
            (
                "com.apple.instruments.dtservicehub".into(),
                RsdServiceInfo { port: 58_784 },
            ),
            (
                "com.apple.dt.testmanagerd.remote".into(),
                RsdServiceInfo { port: 58_793 },
            ),
            (
                "com.apple.coredevice.appservice".into(),
                RsdServiceInfo { port: 58_786 },
            ),
            (
                "com.apple.coredevice.openstdiosocket".into(),
                RsdServiceInfo { port: 58_787 },
            ),
            (
                "com.apple.webinspector.shim.remote".into(),
                RsdServiceInfo { port: 58_788 },
            ),
            (
                "com.apple.coredevice.deviceinfo".into(),
                RsdServiceInfo { port: 58_792 },
            ),
            (
                "com.apple.coredevice.fileservice.control".into(),
                RsdServiceInfo { port: 58_790 },
            ),
            (
                "com.apple.coredevice.fileservice.data".into(),
                RsdServiceInfo { port: 58_791 },
            ),
            (
                "com.apple.accessibility.axAuditDaemon.remoteserver.shim.remote".into(),
                RsdServiceInfo { port: 58_789 },
            ),
        ]);
        if self
            .backend
            .inner
            .debug_proxy_available
            .load(Ordering::SeqCst)
        {
            services.insert(
                "com.apple.internal.dt.remote.debugproxy".into(),
                RsdServiceInfo { port: 58_785 },
            );
        }
        Ok(services)
    }

    async fn connect_rsd_service(&self, service: RsdServiceKind) -> CoreResult<BoxedIo> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(CoreError::DeviceUnavailable("mock tunnel is closed".into()));
        }
        self.ensure_rsd_service(service).await?;
        self.backend.echo_stream().await
    }

    async fn close(&self) -> CoreResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct MockKernelTunnelRunner {
    backend: MockDeviceBackend,
    closed: bool,
}

#[async_trait]
impl KernelTunnelRunner for MockKernelTunnelRunner {
    async fn run(&mut self, cancel: CancellationToken) -> CoreResult<()> {
        if self.closed {
            return Err(CoreError::DeviceUnavailable(
                "mock kernel tunnel is closed".into(),
            ));
        }
        cancel.cancelled().await;
        Ok(())
    }

    async fn close(&mut self) -> CoreResult<()> {
        if !self.closed {
            self.closed = true;
            self.backend
                .inner
                .kernel_tunnel_close_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn mock_service_is_bidirectional_echo() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let mut stream = backend
            .connect_port(&DeviceTarget::default(), 8100)
            .await
            .unwrap();
        stream.write_all(b"hello").await.unwrap();
        let mut response = [0_u8; 5];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"hello");
        assert_eq!(backend.connect_count(), 1);
    }

    #[tokio::test]
    async fn mobilegestalt_plist_is_raw_xml_with_typed_values() {
        let backend = MockDeviceBackend::new();
        let result = backend
            .execute(&CommandInvocation {
                argv: vec![
                    "mobilegestalt".into(),
                    "MockGestalt".into(),
                    "--plist".into(),
                ],
                cwd: None,
                udid: Some("mock-1".into()),
                output: Default::default(),
            })
            .await
            .unwrap();

        assert_eq!(result.value, None);
        let text = result.text.expect("plist text");
        let decoded = plist::Value::from_reader_xml(Cursor::new(text.as_bytes())).unwrap();
        let decoded = decoded.as_dictionary().unwrap();
        assert_eq!(
            decoded.get("Status").and_then(plist::Value::as_string),
            Some("Success")
        );
        assert_eq!(
            decoded
                .get("Diagnostics")
                .and_then(plist::Value::as_dictionary)
                .and_then(|diagnostics| diagnostics.get("MobileGestalt"))
                .and_then(plist::Value::as_dictionary)
                .and_then(|mobilegestalt| mobilegestalt.get("MockGestalt"))
                .and_then(plist::Value::as_boolean),
            Some(true)
        );
    }

    #[tokio::test]
    async fn info_display_requires_and_reuses_deviceinfo_tunnel() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |action: &str| CommandInvocation {
            argv: vec!["info".into(), action.into()],
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };

        let lockdown = backend.execute(&invocation("lockdown")).await.unwrap();
        assert_eq!(lockdown.value.unwrap()["ProductVersion"], "18.0");
        let missing = backend
            .execute_with_tunnel(&invocation("display"), None)
            .await
            .unwrap_err();
        assert!(matches!(missing, CoreError::DeviceUnavailable(_)));

        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(
            backend
                .start_userspace_tunnel(&TunnelSpec {
                    udid: Some("mock-1".into()),
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
        let display = backend
            .execute_with_tunnel(&invocation("display"), Some(tunnel))
            .await
            .unwrap();
        assert_eq!(display.value.unwrap()["displays"][0]["width"], 828);
        assert_eq!(display.warnings, ["reused server-owned shared RSD tunnel"]);
    }

    #[tokio::test]
    async fn one_shot_screenshot_writes_png_and_marks_shared_rsd() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(
            backend
                .start_userspace_tunnel(&TunnelSpec {
                    udid: Some("mock-1".into()),
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
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("capture.png");
        let result = backend
            .execute_with_tunnel(
                &CommandInvocation {
                    argv: vec![
                        "screenshot".into(),
                        format!("--output={}", output.display()),
                    ],
                    cwd: None,
                    udid: Some("mock-1".into()),
                    output: Default::default(),
                },
                Some(tunnel),
            )
            .await
            .unwrap();

        assert_eq!(result.warnings, ["reused server-owned shared RSD tunnel"]);
        let output_text = output.to_string_lossy();
        assert_eq!(
            result.value.unwrap()["output"].as_str(),
            Some(output_text.as_ref())
        );
        assert_eq!(
            std::fs::read(output).unwrap(),
            crate::springboard::MOCK_APP_ICON_PNG
        );
    }

    #[tokio::test]
    async fn mock_ui_runner_tracks_lifecycle_and_serves_health() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let spec = UiRunnerSpec {
            target: rust_ios_protocol::UiRunnerTarget::Wda,
            udid: Some("mock-1".into()),
            runner_bundle_id: "com.example.WDA.xctrunner".into(),
            target_bundle_id: "com.example.WDA.xctrunner".into(),
            xctest_config: "WebDriverAgentRunner.xctest".into(),
            bind_host: "127.0.0.1".into(),
            host_port: 8100,
            device_port: 8100,
            health_path: "/status".into(),
            readiness_timeout_ms: 1_000,
            output_path: None,
        };
        let mut session = backend.start_ui_runner_session(&spec, None).await.unwrap();
        assert_eq!(backend.ui_runner_count(), 1);
        session.ensure_running().await.unwrap();
        assert!(session.try_next_event().unwrap().is_some());

        let mut stream = backend
            .connect_port(&DeviceTarget::default(), 8100)
            .await
            .unwrap();
        stream
            .write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));

        session.close().await.unwrap();
        assert_eq!(backend.ui_runner_close_count(), 1);
    }

    #[tokio::test]
    async fn detach_changes_device_snapshot() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        assert_eq!(backend.list_devices().await.unwrap().devices.len(), 1);
        backend.detach("mock-1").await;
        assert!(backend.list_devices().await.unwrap().devices.is_empty());
    }

    #[tokio::test]
    async fn mock_activate_is_idempotent_and_rejects_extra_arguments() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };

        let result = backend.execute(&invocation(&["activate"])).await.unwrap();
        let value = result.value.unwrap();
        assert_eq!(value["status"], "already_activated");
        assert_eq!(value["activationState"], "Activated");
        assert_eq!(value["verified"], true);
        assert_eq!(value["udid"], "mock-1");
        assert!(matches!(
            backend
                .execute(&invocation(&["activate", "unexpected"]))
                .await,
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn mock_erase_enforces_irreversible_operation_gate() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str], udid: Option<&str>| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: udid.map(str::to_owned),
            output: Default::default(),
        };

        for invalid in [
            invocation(&["erase"], Some("mock-1")),
            invocation(&["erase", "--force"], None),
        ] {
            assert!(matches!(
                backend.execute(&invalid).await,
                Err(CoreError::InvalidArgument(_))
            ));
        }
        let result = backend
            .execute(&invocation(&["erase", "--force"], Some("mock-1")))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(result["status"], "erase_requested");
        assert_eq!(result["preserveDataPlan"], true);
        assert_eq!(result["udid"], "mock-1");
    }

    #[tokio::test]
    async fn mock_set_wallpaper_uses_shared_parser_and_protocol_values() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };
        assert!(matches!(
            backend
                .execute(&invocation(&[
                    "set-wallpaper",
                    "wallpaper.png",
                    "--screen=invalid",
                    "--p12file=identity.p12",
                ]))
                .await,
            Err(CoreError::InvalidArgument(_))
        ));
        let value = backend
            .execute(&invocation(&[
                "set-wallpaper",
                "wallpaper.png",
                "--screen=lock",
                "--p12file=identity.p12",
            ]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(value["status"], "set");
        assert_eq!(value["screen"], "lock");
        assert_eq!(value["where"], 1);
        assert_eq!(value["supervised"], true);
    }

    #[tokio::test]
    async fn mock_pasteboard_get_and_set_have_stable_shapes() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };

        let value = backend
            .execute(&invocation(&["pasteboard", "get"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(value["pasteboard"], "general");
        assert_eq!(value["text"], "mock pasteboard text");
        assert_eq!(value["items"], 1);
        assert_eq!(value["serviceTransport"], "mock");

        let value = backend
            .execute(&invocation(&["pasteboard", "set", "hello"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(value["status"], "set");
        // 文本不得回显到结果，避免剪贴板内容进入日志。
        assert!(value.get("text").is_none());

        assert!(matches!(
            backend.execute(&invocation(&["pasteboard", "copy"])).await,
            Err(CoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            backend.execute(&invocation(&["pasteboard"])).await,
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn mock_mdm_requires_identity_and_supports_four_actions() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };

        // 缺 --p12file 必须在执行前拒绝。
        for missing in [
            invocation(&["mdm", "security-info"]),
            invocation(&["mdm", "fetch-unlock-token", "--output", "token.bin"]),
        ] {
            assert!(matches!(
                backend.execute(&missing).await,
                Err(CoreError::InvalidArgument(_))
            ));
        }
        // 未知 action。
        assert!(matches!(
            backend
                .execute(&invocation(&["mdm", "nuke", "--p12file", "id.p12"]))
                .await,
            Err(CoreError::InvalidArgument(_))
        ));

        let value = backend
            .execute(&invocation(&[
                "mdm",
                "security-info",
                "--p12file",
                "identity.p12",
            ]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(value["securityInfo"]["PasscodePresent"], false);
        assert_eq!(value["serviceTransport"], "mock");

        let value = backend
            .execute(&invocation(&[
                "mdm",
                "fetch-unlock-token",
                "--p12file",
                "identity.p12",
                "--output",
                "unlock-token.bin",
            ]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(value["status"], "fetched");
        assert_eq!(value["bytes"], 32);

        for action in ["clear-passcode", "clear-screen-time-password"] {
            let value = backend
                .execute(&invocation(&["mdm", action, "--p12file", "identity.p12"]))
                .await
                .unwrap()
                .value
                .unwrap();
            assert_eq!(value["status"], "cleared");
        }
    }

    #[tokio::test]
    async fn mock_pair_supports_standard_and_supervised_contracts() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invocation = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };
        let standard = backend
            .execute(&invocation(&["pair", "--name=lab-host"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(standard["supervised"], false);
        assert_eq!(standard["hostName"], "lab-host");
        let supervised = backend
            .execute(&invocation(&[
                "pair",
                "--p12file=organization.p12",
                "--password=secret",
            ]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(supervised["supervised"], true);
        assert_eq!(supervised["saved"], true);
        assert_eq!(supervised["udid"], "mock-1");
    }

    #[tokio::test]
    async fn mock_tunnel_exposes_info_and_stream() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let tunnel = backend
            .start_userspace_tunnel(&TunnelSpec {
                udid: None,
                mode: rust_ios_protocol::TunnelMode::Userspace,
                transport: TunnelTransport::UsbCoreDevice,
                pair_record_path: "default".into(),
                userspace_bind_host: "127.0.0.1".into(),
                userspace_port: 0,
                kernel_interface_name: None,
                wintun_path: None,
            })
            .await
            .unwrap();
        assert_eq!(tunnel.info().udid, "mock-1");
        assert!(
            tunnel
                .rsd_services()
                .await
                .unwrap()
                .contains_key("com.apple.internal.dt.remote.debugproxy")
        );
        let mut stream = tunnel.connect(62078).await.unwrap();
        stream.write_all(b"rsd").await.unwrap();
        let mut bytes = [0_u8; 3];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"rsd");
        tunnel.close().await.unwrap();
        assert!(tunnel.connect(1).await.is_err());
    }

    #[tokio::test]
    async fn mock_kernel_tunnel_is_cancellable_and_closes_once() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let mut handle = backend
            .start_kernel_tunnel(&TunnelSpec {
                udid: Some("mock-1".into()),
                mode: rust_ios_protocol::TunnelMode::Kernel,
                transport: TunnelTransport::UsbCoreDevice,
                pair_record_path: "default".into(),
                userspace_bind_host: "127.0.0.1".into(),
                userspace_port: 0,
                kernel_interface_name: Some("rust-ios-test".into()),
                wintun_path: Some("C:\\Windows\\System32\\wintun.dll".into()),
            })
            .await
            .unwrap();
        assert_eq!(handle.info.interface_name, "rust-ios-test");
        assert_eq!(backend.kernel_tunnel_count(), 1);
        let mut stream = handle.route.connect(58_783).await.unwrap();
        stream.write_all(b"kernel").await.unwrap();
        let mut bytes = [0_u8; 6];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"kernel");

        let cancel = CancellationToken::new();
        cancel.cancel();
        handle.runner.run(cancel).await.unwrap();
        handle.runner.close().await.unwrap();
        handle.runner.close().await.unwrap();
        handle.route.close().await.unwrap();
        assert_eq!(backend.kernel_tunnel_close_count(), 1);
        assert!(handle.route.connect(1).await.is_err());
    }

    #[tokio::test]
    async fn mock_webinspector_has_list_eval_and_launch_paths() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let invoke = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
            cwd: None,
            udid: Some("mock-1".into()),
            output: Default::default(),
        };

        let list = backend
            .execute(&invoke(&["webinspector", "list"]))
            .await
            .unwrap();
        assert_eq!(list.value.unwrap()[0]["page"]["key"], "mock-page-1");
        let evaluated = backend
            .execute(&invoke(&["webinspector", "eval", "mock-page-1", "2+2"]))
            .await
            .unwrap();
        assert_eq!(evaluated.value, Some(json!(4)));
        let launched = backend
            .execute(&invoke(&["webinspector", "launch", "https://example.com/"]))
            .await
            .unwrap();
        assert_eq!(launched.value.unwrap()["title"], "Mock Page");
    }

    #[tokio::test]
    async fn mock_device_condition_is_closed_idempotently() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let condition = backend
            .start_device_condition(
                &DeviceStateSpec {
                    udid: Some("mock-1".into()),
                    condition_identifier: "SlowNetworkCondition".into(),
                    profile_identifier: "SlowNetwork3GGood".into(),
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(condition.info().udid, "mock-1");
        assert_eq!(backend.condition_count(), 1);
        condition.close().await.unwrap();
        condition.close().await.unwrap();
        assert_eq!(backend.condition_close_count(), 1);
    }

    #[tokio::test]
    async fn mock_continuous_streams_are_deterministic() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let mut syslog = backend
            .start_stream(
                &StreamTaskSpec {
                    kind: TaskKind::Syslog,
                    udid: Some("mock-1".into()),
                    output_path: None,
                    format: StreamFormat::JsonLines,
                    pid: None,
                    process: None,
                    os_trace: None,
                    watch_kill_targets: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(syslog.info().content_type, "application/x-ndjson");
        let line = String::from_utf8(syslog.next_chunk().await.unwrap()).unwrap();
        assert!(line.contains("mock syslog mock-1 1"));

        let mut ostrace = backend
            .start_stream(
                &StreamTaskSpec {
                    kind: TaskKind::OsTrace,
                    udid: Some("mock-1".into()),
                    output_path: None,
                    format: StreamFormat::Raw,
                    pid: Some(42),
                    process: None,
                    os_trace: None,
                    watch_kill_targets: None,
                },
                None,
            )
            .await
            .unwrap();
        let line = String::from_utf8(ostrace.next_chunk().await.unwrap()).unwrap();
        assert!(line.contains("mock ostrace mock-1 1"));

        let mut filtered_ostrace = backend
            .start_stream(
                &StreamTaskSpec {
                    kind: TaskKind::OsTrace,
                    udid: Some("mock-1".into()),
                    output_path: None,
                    format: StreamFormat::JsonLines,
                    pid: None,
                    process: None,
                    os_trace: Some(OsTraceOptions {
                        levels: vec![OsTraceLevel::Error, OsTraceLevel::Fault],
                        subsystem_contains: Some("com.example.storage".into()),
                        message_contains: Some("timeout".into()),
                        message_excludes: Some("ignored".into()),
                        follow: true,
                    }),
                    watch_kill_targets: None,
                },
                None,
            )
            .await
            .unwrap();
        let filtered: Value =
            serde_json::from_slice(&filtered_ostrace.next_chunk().await.unwrap()).unwrap();
        assert_eq!(filtered["level"], "Error");
        assert_eq!(filtered["label"]["subsystem"], "com.example.storage");
        assert!(filtered["message"].as_str().unwrap().contains("timeout"));
        assert!(!filtered["message"].as_str().unwrap().contains("ignored"));

        for (kind, expected) in [
            (TaskKind::Notifications, "mock notifications mock-1 1"),
            (TaskKind::Sysmontap, "mock sysmontap mock-1 1"),
        ] {
            let mut stream = backend
                .start_stream(
                    &StreamTaskSpec {
                        kind,
                        udid: Some("mock-1".into()),
                        output_path: None,
                        format: StreamFormat::JsonLines,
                        pid: None,
                        process: None,
                        os_trace: None,
                        watch_kill_targets: None,
                    },
                    None,
                )
                .await
                .unwrap();
            let line = String::from_utf8(stream.next_chunk().await.unwrap()).unwrap();
            assert!(line.contains(expected));
        }

        let mut pcap = backend
            .start_stream(
                &StreamTaskSpec {
                    kind: TaskKind::Pcap,
                    udid: Some("mock-1".into()),
                    output_path: None,
                    format: StreamFormat::Pcap,
                    pid: Some(42),
                    process: Some("App".into()),
                    os_trace: None,
                    watch_kill_targets: None,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(pcap.info().initial_bytes.len(), 24);
        assert_eq!(&pcap.info().initial_bytes[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert!(pcap.next_chunk().await.unwrap().len() > 16);
    }

    #[tokio::test]
    async fn mock_accessibility_inspector_emits_ndjson_and_closes() {
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-1").await;
        let mut stream = backend
            .start_accessibility_inspector(&AccessibilityInspectorSpec {
                udid: Some("mock-1".into()),
                output_path: None,
                font_size: Some(17.0),
            })
            .await
            .unwrap();
        assert_eq!(stream.info().content_type, "application/x-ndjson");
        let started: Value = serde_json::from_slice(&stream.next_chunk().await.unwrap()).unwrap();
        assert_eq!(started["event"], "inspector_started");
        assert_eq!(started["fontSize"], 17.0);
        let focus: Value = serde_json::from_slice(&stream.next_chunk().await.unwrap()).unwrap();
        assert_eq!(focus["selector"], "hostInspectorCurrentElementChanged:");
        assert_eq!(focus["data"]["udid"], "mock-1");
        stream.close().await.unwrap();
        assert!(matches!(
            stream.next_chunk().await,
            Err(CoreError::DeviceUnavailable(_))
        ));
    }

    #[tokio::test]
    async fn mock_lockdown_settings_have_stable_shapes() {
        let backend = MockDeviceBackend::new();
        let invoke = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).into()).collect(),
            cwd: None,
            udid: None,
            output: rust_ios_protocol::OutputOptions::default(),
        };

        let language = backend
            .execute(&invoke(&["lang", "--setlang=zh-Hans"]))
            .await
            .unwrap();
        assert_eq!(language.value.unwrap()["Language"], "zh-Hans");

        let assistive = backend
            .execute(&invoke(&["assistivetouch", "get"]))
            .await
            .unwrap();
        assert_eq!(assistive.value.unwrap()["AssistiveTouchEnabled"], false);

        let time = backend
            .execute(&invoke(&["timeformat", "24h"]))
            .await
            .unwrap();
        assert_eq!(time.value.unwrap()["TimeFormat"], "24h");

        let date = backend.execute(&invoke(&["date"])).await.unwrap();
        assert_eq!(date.value.as_ref().unwrap()["TimeIntervalSince1970"], 0.0);
        assert_eq!(
            date.value.as_ref().unwrap()["formatedDate"],
            "Thursday, 01-Jan-70 00:00:00 UTC"
        );
        assert_eq!(
            date.text.as_deref(),
            Some("Thursday, 01-Jan-70 00:00:00 UTC")
        );

        let diagnostics = backend
            .execute(&invoke(&["diagnostics", "list"]))
            .await
            .unwrap();
        assert_eq!(diagnostics.value.as_ref().unwrap()["Status"], "Success");
        assert_eq!(
            diagnostics.value.as_ref().unwrap()["Diagnostics"]["IORegistry"]["IsCharging"],
            false
        );
        assert!(backend.execute(&invoke(&["diagnostics"])).await.is_err());
        assert!(
            backend
                .execute(&invoke(&["timeformat", "enable"]))
                .await
                .is_err()
        );

        let image = backend
            .execute(&invoke(&["image", "auto", "--basedir=devimages"]))
            .await
            .unwrap();
        assert_eq!(image.value.unwrap()["buildVersion"], "23F84");
    }

    #[tokio::test]
    async fn mock_accessibility_commands_have_stable_shapes() {
        let backend = MockDeviceBackend::new();
        let invoke = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).into()).collect(),
            cwd: None,
            udid: None,
            output: rust_ios_protocol::OutputOptions::default(),
        };

        let audit = backend
            .execute(&invoke(&["ax", "audit"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(audit["apiVersion"], 21);
        assert_eq!(audit["issueCount"], 1);
        assert_eq!(audit["issues"][0]["issueType"], "testTypeContrast");

        let reset = backend
            .execute(&invoke(&["resetax"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(reset["status"], "reset");
    }

    #[tokio::test]
    async fn mock_app_icon_writes_a_client_relative_png() {
        let backend = MockDeviceBackend::new();
        let directory = tempfile::tempdir().unwrap();
        let invoke = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).into()).collect(),
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            udid: None,
            output: rust_ios_protocol::OutputOptions::default(),
        };

        let icon = backend
            .execute(&invoke(&["get-app-icon", "com.apple.Preferences"]))
            .await
            .unwrap()
            .value
            .unwrap();
        let output = directory.path().join("com.apple.Preferences.png");
        assert_eq!(icon["bundleID"], "com.apple.Preferences");
        assert_eq!(icon["bytes"], MOCK_APP_ICON_PNG.len());
        assert_eq!(std::fs::read(output).unwrap(), MOCK_APP_ICON_PNG);

        assert!(matches!(
            backend.execute(&invoke(&["get-app-icon"])).await,
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn mock_ui_supports_command_and_file_output_shapes() {
        let backend = MockDeviceBackend::new();
        let directory = tempfile::tempdir().unwrap();
        let invoke = |argv: &[&str]| CommandInvocation {
            argv: argv.iter().map(|value| (*value).into()).collect(),
            cwd: Some(directory.path().to_string_lossy().into_owned()),
            udid: None,
            output: rust_ios_protocol::OutputOptions::default(),
        };

        let status = backend
            .execute(&invoke(&["ui", "status", "--driver=wda"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(status["driver"], "wda");
        assert_eq!(status["ready"], true);

        let screenshot = backend
            .execute(&invoke(&["ui", "screenshot", "--output=capture.png"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(screenshot["bytes"], 8);
        assert_eq!(
            std::fs::read(directory.path().join("capture.png")).unwrap(),
            b"\x89PNG\r\n\x1a\n"
        );

        let source = backend
            .execute(&invoke(&["ui", "source", "--output=hierarchy.xml"]))
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(source["bytes"], 10);
        assert_eq!(
            std::fs::read(directory.path().join("hierarchy.xml")).unwrap(),
            b"<MockUI />"
        );
    }
}
