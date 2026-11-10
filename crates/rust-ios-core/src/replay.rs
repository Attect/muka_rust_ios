use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult, ConnectionType,
    DeviceInfo, DeviceList, DeviceStateSpec, LocationSimulationSpec, RemotePairingInfo,
    StreamTaskSpec, TunnelPairSpec, TunnelSpec, UiRunnerSpec, WdaSpec, WebInspectorCdpSpec,
    WebInspectorJsShellSpec, XCTestSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    AppProcessSession, BoxedIo, CoreError, CoreResult, DeviceBackend, DeviceCondition,
    DeviceLocationSimulation, DeviceStream, DeviceTarget, KernelTunnelHandle, MockDeviceBackend,
    UiRunnerSession, UserSpaceTunnel, WdaSession, WebInspectorCdpSession,
    WebInspectorJsShellSession, XCTestSession,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayFixture {
    #[serde(default = "default_fixture_name")]
    pub name: String,
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
    #[serde(default = "default_buid")]
    pub buid: String,
    #[serde(default)]
    pub pair_records: BTreeMap<String, Value>,
    #[serde(default)]
    pub commands: Vec<ReplayCommand>,
}

fn replay_command_uses_shared_rsd(invocation: &CommandInvocation) -> bool {
    match invocation.argv.first().map(String::as_str) {
        Some("file") => crate::file::has_domain_selector(&invocation.argv),
        Some(
            "ps" | "launch" | "kill" | "memlimitoff" | "devicestate" | "webinspector"
            | "screenshot" | "setlocation" | "resetlocation" | "image" | "hid",
        ) => true,
        _ => false,
    }
}

fn default_fixture_name() -> String {
    "unnamed-replay".into()
}

fn default_buid() -> String {
    "REPLAY-BUID".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayCommand {
    /// argv 前缀；最长匹配优先。
    pub argv: Vec<String>,
    pub result: CommandResult,
}

#[derive(Debug)]
pub struct ReplayDeviceBackend {
    fixture: ReplayFixture,
    transport: MockDeviceBackend,
    calls: Mutex<Vec<Vec<String>>>,
}

impl ReplayDeviceBackend {
    pub async fn from_path(path: impl AsRef<Path>) -> CoreResult<Self> {
        let bytes = tokio::fs::read(path).await?;
        let fixture = serde_json::from_slice(&bytes)
            .map_err(|error| CoreError::Backend(format!("invalid replay fixture: {error}")))?;
        Self::from_fixture(fixture).await
    }

    pub async fn from_fixture(fixture: ReplayFixture) -> CoreResult<Self> {
        let transport = MockDeviceBackend::new();
        for device in &fixture.devices {
            match device.connection_type {
                ConnectionType::Usb | ConnectionType::Unknown => {
                    transport.attach_usb(device.udid.clone()).await;
                }
                ConnectionType::Network => {
                    transport
                        .attach_network(
                            device.udid.clone(),
                            device
                                .network_address
                                .clone()
                                .unwrap_or_else(|| "127.0.0.1".into()),
                        )
                        .await;
                }
            }
        }
        Ok(Self {
            fixture,
            transport,
            calls: Mutex::new(Vec::new()),
        })
    }

    pub async fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl DeviceBackend for ReplayDeviceBackend {
    fn name(&self) -> &'static str {
        "replay"
    }

    async fn list_devices(&self) -> CoreResult<DeviceList> {
        Ok(DeviceList {
            devices: self.fixture.devices.clone(),
        })
    }

    async fn read_buid(&self) -> CoreResult<String> {
        Ok(self.fixture.buid.clone())
    }

    async fn read_pair_record(&self, udid: &str) -> CoreResult<Value> {
        self.fixture.pair_records.get(udid).cloned().ok_or_else(|| {
            CoreError::PairingRequired(format!("fixture has no pair record for {udid}"))
        })
    }

    async fn connect_port(&self, target: &DeviceTarget, port: u16) -> CoreResult<BoxedIo> {
        self.transport.connect_port(target, port).await
    }

    async fn start_userspace_tunnel(
        &self,
        spec: &TunnelSpec,
    ) -> CoreResult<Box<dyn UserSpaceTunnel>> {
        self.transport.start_userspace_tunnel(spec).await
    }

    async fn start_kernel_tunnel(&self, spec: &TunnelSpec) -> CoreResult<KernelTunnelHandle> {
        self.transport.start_kernel_tunnel(spec).await
    }

    async fn pair_remote_tunnel(&self, spec: &TunnelPairSpec) -> CoreResult<RemotePairingInfo> {
        Err(CoreError::NotImplemented(format!(
            "replay fixture cannot perform RemotePairing consent for {:?}",
            spec.udid
        )))
    }

    async fn start_stream(
        &self,
        spec: &StreamTaskSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        self.transport.start_stream(spec, tunnel).await
    }

    async fn start_accessibility_inspector(
        &self,
        spec: &AccessibilityInspectorSpec,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        self.transport.start_accessibility_inspector(spec).await
    }

    async fn start_device_condition(
        &self,
        spec: &DeviceStateSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceCondition>> {
        self.transport.start_device_condition(spec, tunnel).await
    }

    async fn start_location_simulation(
        &self,
        spec: &LocationSimulationSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceLocationSimulation>> {
        self.transport.start_location_simulation(spec, tunnel).await
    }

    async fn start_wda_session(
        &self,
        spec: &WdaSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WdaSession>> {
        self.transport.start_wda_session(spec, tunnel).await
    }

    async fn start_ui_runner_session(
        &self,
        spec: &UiRunnerSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn UiRunnerSession>> {
        self.transport.start_ui_runner_session(spec, tunnel).await
    }

    async fn start_xctest_session(
        &self,
        spec: &XCTestSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn XCTestSession>> {
        self.transport.start_xctest_session(spec, tunnel).await
    }

    async fn start_app_process_session(
        &self,
        spec: &AppLaunchSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn AppProcessSession>> {
        self.transport.start_app_process_session(spec, tunnel).await
    }

    async fn start_webinspector_cdp_session(
        &self,
        spec: &WebInspectorCdpSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorCdpSession>> {
        self.transport
            .start_webinspector_cdp_session(spec, tunnel)
            .await
    }

    async fn start_webinspector_js_shell_session(
        &self,
        spec: &WebInspectorJsShellSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorJsShellSession>> {
        self.transport
            .start_webinspector_js_shell_session(spec, tunnel)
            .await
    }

    async fn execute(&self, invocation: &CommandInvocation) -> CoreResult<CommandResult> {
        self.calls.lock().await.push(invocation.argv.clone());
        let mut commands: Vec<_> = self
            .fixture
            .commands
            .iter()
            .filter(|command| invocation.argv.starts_with(&command.argv))
            .collect();
        commands.sort_by_key(|command| std::cmp::Reverse(command.argv.len()));
        commands
            .first()
            .map(|command| command.result.clone())
            .ok_or_else(|| {
                CoreError::NotImplemented(format!(
                    "replay fixture {:?} has no result for {:?}",
                    self.fixture.name, invocation.argv
                ))
            })
    }

    async fn execute_with_tunnel(
        &self,
        invocation: &CommandInvocation,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<CommandResult> {
        if replay_command_uses_shared_rsd(invocation) && tunnel.is_some() {
            let mut result = self.execute(invocation).await?;
            result
                .warnings
                .push("reused server-owned shared RSD tunnel".into());
            Ok(result)
        } else {
            self.execute(invocation).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rust_ios_protocol::{ConnectionPreference, OutputOptions};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn fixture() -> ReplayFixture {
        ReplayFixture {
            name: "unit".into(),
            devices: vec![DeviceInfo {
                device_id: 7,
                udid: "replay-phone".into(),
                connection_type: ConnectionType::Usb,
                network_address: None,
                properties: BTreeMap::new(),
            }],
            buid: "BUID-REPLAY".into(),
            pair_records: BTreeMap::from([("replay-phone".into(), json!({ "HostID": "host" }))]),
            commands: vec![
                ReplayCommand {
                    argv: vec!["info".into()],
                    result: CommandResult {
                        exit_code: 0,
                        value: Some(json!({ "ProductVersion": "18.0" })),
                        text: None,
                        warnings: Vec::new(),
                    },
                },
                ReplayCommand {
                    argv: vec!["diagnostics".into(), "list".into()],
                    result: CommandResult {
                        exit_code: 0,
                        value: Some(json!({
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
                        })),
                        text: None,
                        warnings: Vec::new(),
                    },
                },
                ReplayCommand {
                    argv: vec!["ps".into()],
                    result: CommandResult {
                        exit_code: 0,
                        value: Some(json!([])),
                        text: None,
                        warnings: Vec::new(),
                    },
                },
                ReplayCommand {
                    argv: vec!["file".into(), "ls".into(), "--temp".into()],
                    result: CommandResult {
                        exit_code: 0,
                        value: Some(json!({
                            "path": ".",
                            "files": [],
                            "count": 0,
                            "domain": "temporary",
                            "serviceTransport": "shared_rsd",
                        })),
                        text: None,
                        warnings: Vec::new(),
                    },
                },
            ],
        }
    }

    #[tokio::test]
    async fn fixture_replays_control_and_echo_data_plane() {
        let backend = ReplayDeviceBackend::from_fixture(fixture()).await.unwrap();
        let result = backend
            .execute(&CommandInvocation {
                argv: vec!["info".into()],
                cwd: None,
                udid: Some("replay-phone".into()),
                output: OutputOptions::default(),
            })
            .await
            .unwrap();
        assert_eq!(result.value.unwrap()["ProductVersion"], "18.0");
        let diagnostics = backend
            .execute(&CommandInvocation {
                argv: vec!["diagnostics".into(), "list".into()],
                cwd: None,
                udid: Some("replay-phone".into()),
                output: OutputOptions::default(),
            })
            .await
            .unwrap();
        assert_eq!(diagnostics.value.unwrap()["Status"], "Success");
        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(
            backend
                .start_userspace_tunnel(&TunnelSpec {
                    udid: Some("replay-phone".into()),
                    mode: rust_ios_protocol::TunnelMode::Userspace,
                    transport: rust_ios_protocol::TunnelTransport::UsbCoreDevice,
                    pair_record_path: "default".into(),
                    userspace_bind_host: "127.0.0.1".into(),
                    userspace_port: 0,
                    kernel_interface_name: None,
                    wintun_path: None,
                })
                .await
                .unwrap(),
        );
        let result = backend
            .execute_with_tunnel(
                &CommandInvocation {
                    argv: vec!["ps".into()],
                    cwd: None,
                    udid: Some("replay-phone".into()),
                    output: OutputOptions::default(),
                },
                Some(tunnel.clone()),
            )
            .await
            .unwrap();
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );
        let result = backend
            .execute_with_tunnel(
                &CommandInvocation {
                    argv: vec!["file".into(), "ls".into(), "--temp".into()],
                    cwd: None,
                    udid: Some("replay-phone".into()),
                    output: OutputOptions::default(),
                },
                Some(tunnel.clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap()["domain"], "temporary");
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );
        let mut stream = backend
            .connect_port(
                &DeviceTarget {
                    udid: Some("replay-phone".into()),
                    connection: ConnectionPreference::UsbPreferred,
                },
                8100,
            )
            .await
            .unwrap();
        stream.write_all(b"recorded").await.unwrap();
        let mut bytes = [0_u8; 8];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"recorded");
        assert_eq!(
            backend.calls().await,
            vec![
                vec!["info".to_owned()],
                vec!["diagnostics".to_owned(), "list".to_owned()],
                vec!["ps".to_owned()],
                vec!["file".to_owned(), "ls".to_owned(), "--temp".to_owned()],
            ]
        );
    }
}
