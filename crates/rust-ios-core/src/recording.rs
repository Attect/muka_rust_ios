use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult, DeviceList,
    DeviceStateSpec, LocationSimulationSpec, RemotePairingInfo, StreamTaskSpec, TunnelPairSpec,
    TunnelSpec, UiRunnerSpec, WdaSpec, WebInspectorCdpSpec, WebInspectorJsShellSpec, XCTestSpec,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::{
    AppProcessSession, BoxedIo, CoreResult, DeviceBackend, DeviceCondition,
    DeviceLocationSimulation, DeviceStream, DeviceTarget, KernelTunnelHandle, UiRunnerSession,
    UserSpaceTunnel, WdaSession, WebInspectorCdpSession, WebInspectorJsShellSession, XCTestSession,
};

#[derive(Debug)]
pub struct RecordingBackend {
    inner: Arc<dyn DeviceBackend>,
    path: PathBuf,
    writer_lock: Mutex<()>,
}

#[derive(Debug, Serialize)]
struct RecordedEvent<'a> {
    timestamp_ms: u64,
    backend: &'static str,
    operation: &'a str,
    request: Value,
    outcome: Value,
}

impl RecordingBackend {
    pub fn new(inner: Arc<dyn DeviceBackend>, path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            path: path.into(),
            writer_lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn record(&self, operation: &str, request: Value, outcome: Value) {
        let _guard = self.writer_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let event = RecordedEvent {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_millis() as u64,
            backend: self.inner.name(),
            operation,
            request,
            outcome,
        };
        let Ok(mut bytes) = serde_json::to_vec(&event) else {
            return;
        };
        bytes.push(b'\n');
        if let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
        {
            let _ = file.write_all(&bytes).await;
            let _ = file.flush().await;
        }
    }
}

#[async_trait]
impl DeviceBackend for RecordingBackend {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn list_devices(&self) -> CoreResult<DeviceList> {
        let result = self.inner.list_devices().await;
        self.record(
            "list_devices",
            Value::Null,
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |devices| serde_json::to_value(devices).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn read_buid(&self) -> CoreResult<String> {
        let result = self.inner.read_buid().await;
        self.record(
            "read_buid",
            Value::Null,
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |buid| json!({ "buid": buid }),
            ),
        )
        .await;
        result
    }

    async fn read_pair_record(&self, udid: &str) -> CoreResult<Value> {
        let result = self.inner.read_pair_record(udid).await;
        // PairRecord 含私钥，录制文件只记录成功/失败，绝不写入实际内容。
        self.record(
            "read_pair_record",
            json!({ "udid": udid }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |_| json!({ "success": true, "redacted": true }),
            ),
        )
        .await;
        result
    }

    async fn connect_port(&self, target: &DeviceTarget, port: u16) -> CoreResult<BoxedIo> {
        let result = self.inner.connect_port(target, port).await;
        self.record(
            "connect_port",
            json!({ "udid": target.udid, "connection": target.connection, "port": port }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |_| json!({ "success": true, "dataPlane": "not_recorded" }),
            ),
        )
        .await;
        result
    }

    async fn start_userspace_tunnel(
        &self,
        spec: &TunnelSpec,
    ) -> CoreResult<Box<dyn UserSpaceTunnel>> {
        let result = self.inner.start_userspace_tunnel(spec).await;
        self.record(
            "start_userspace_tunnel",
            json!({
                "udid": spec.udid,
                "mode": spec.mode,
                "transport": spec.transport,
                "pairRecordPath": spec.pair_record_path,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |tunnel| serde_json::to_value(tunnel.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_kernel_tunnel(&self, spec: &TunnelSpec) -> CoreResult<KernelTunnelHandle> {
        let result = self.inner.start_kernel_tunnel(spec).await;
        self.record(
            "start_kernel_tunnel",
            json!({
                "udid": spec.udid,
                "mode": spec.mode,
                "transport": spec.transport,
                "kernelInterfaceName": spec.kernel_interface_name,
                "wintunPath": spec.wintun_path,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |handle| serde_json::to_value(&handle.info).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn pair_remote_tunnel(&self, spec: &TunnelPairSpec) -> CoreResult<RemotePairingInfo> {
        let result = self.inner.pair_remote_tunnel(spec).await;
        self.record(
            "pair_remote_tunnel",
            json!({
                "udid": spec.udid,
                "pairRecordPath": spec.pair_record_path,
                "manual": spec.manual,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |info| serde_json::to_value(info).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_stream(
        &self,
        spec: &StreamTaskSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_stream(spec, tunnel).await;
        self.record(
            "start_stream",
            json!({
                "spec": spec,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |stream| serde_json::to_value(stream.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_accessibility_inspector(
        &self,
        spec: &AccessibilityInspectorSpec,
    ) -> CoreResult<Box<dyn DeviceStream>> {
        let result = self.inner.start_accessibility_inspector(spec).await;
        self.record(
            "start_accessibility_inspector",
            serde_json::to_value(spec).unwrap_or(Value::Null),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |stream| serde_json::to_value(stream.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_device_condition(
        &self,
        spec: &DeviceStateSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceCondition>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_device_condition(spec, tunnel).await;
        self.record(
            "start_device_condition",
            json!({
                "spec": spec,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |condition| serde_json::to_value(condition.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_location_simulation(
        &self,
        spec: &LocationSimulationSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceLocationSimulation>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_location_simulation(spec, tunnel).await;
        // 模拟位置属于敏感数据；录制文件只保留数量和 transport，不写坐标、时间或路径。
        self.record(
            "start_location_simulation",
            json!({
                "udid": spec.udid,
                "pointCount": spec.points.len(),
                "sharedRsd": shared_rsd,
                "locationDataRedacted": true,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_wda_session(
        &self,
        spec: &WdaSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WdaSession>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_wda_session(spec, tunnel).await;
        self.record(
            "start_wda_session",
            json!({
                "udid": spec.udid,
                "runnerBundleId": spec.runner_bundle_id,
                "targetBundleId": spec.target_bundle_id,
                "xctestConfig": spec.xctest_config,
                "argumentCount": spec.arguments.len(),
                "environmentKeys": spec.environment.keys().collect::<Vec<_>>(),
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_ui_runner_session(
        &self,
        spec: &UiRunnerSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn UiRunnerSession>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_ui_runner_session(spec, tunnel).await;
        self.record(
            "start_ui_runner_session",
            json!({
                "target": spec.target,
                "udid": spec.udid,
                "runnerBundleId": spec.runner_bundle_id,
                "targetBundleId": spec.target_bundle_id,
                "xctestConfig": spec.xctest_config,
                "devicePort": spec.device_port,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_xctest_session(
        &self,
        spec: &XCTestSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn XCTestSession>> {
        let shared_rsd = tunnel.is_some();
        let result = self.inner.start_xctest_session(spec, tunnel).await;
        self.record(
            "start_xctest_session",
            json!({
                "udid": spec.udid,
                "targetCount": spec.targets.len(),
                "runnerBundleIds": spec.targets.iter()
                    .map(|target| target.runner_bundle_id.as_str())
                    .collect::<Vec<_>>(),
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_app_process_session(
        &self,
        spec: &AppLaunchSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn AppProcessSession>> {
        let result = self.inner.start_app_process_session(spec, tunnel).await;
        self.record(
            "start_app_process_session",
            json!({
                "udid": spec.udid,
                "bundleId": spec.bundle_id,
                "arguments": spec.arguments,
                "environmentKeys": spec.environment.keys().collect::<Vec<_>>(),
                "killExisting": spec.kill_existing,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_webinspector_cdp_session(
        &self,
        spec: &WebInspectorCdpSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorCdpSession>> {
        let shared_rsd = tunnel.is_some();
        let result = self
            .inner
            .start_webinspector_cdp_session(spec, tunnel)
            .await;
        self.record(
            "start_webinspector_cdp_session",
            json!({
                "udid": spec.udid,
                "bindHost": spec.bind_host,
                "port": spec.port,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn start_webinspector_js_shell_session(
        &self,
        spec: &WebInspectorJsShellSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorJsShellSession>> {
        let shared_rsd = tunnel.is_some();
        let result = self
            .inner
            .start_webinspector_js_shell_session(spec, tunnel)
            .await;
        self.record(
            "start_webinspector_js_shell_session",
            json!({
                "udid": spec.udid,
                "url": spec.url,
                "bundleId": spec.bundle_id,
                "openSafari": spec.open_safari,
                "commandTimeoutMs": spec.command_timeout_ms,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |session| serde_json::to_value(session.info()).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn execute(&self, invocation: &CommandInvocation) -> CoreResult<CommandResult> {
        let result = self.inner.execute(invocation).await;
        self.record(
            "execute",
            json!({
                "argv": redact_command_arguments(&invocation.argv),
                "cwd": invocation.cwd,
                "udid": invocation.udid,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |result| serde_json::to_value(result).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }

    async fn execute_with_tunnel(
        &self,
        invocation: &CommandInvocation,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<CommandResult> {
        if tunnel.is_none() {
            return self.execute(invocation).await;
        }
        let shared_rsd = tunnel.is_some();
        let result = self.inner.execute_with_tunnel(invocation, tunnel).await;
        self.record(
            "execute_with_tunnel",
            json!({
                "invocation": invocation,
                "sharedRsd": shared_rsd,
            }),
            result.as_ref().map_or_else(
                |error| json!({ "error": error.to_string() }),
                |result| serde_json::to_value(result).unwrap_or(Value::Null),
            ),
        )
        .await;
        result
    }
}

fn redact_command_arguments(arguments: &[String]) -> Vec<String> {
    const SENSITIVE_OPTIONS: &[&str] = &["--password", "--p12password"];
    let command = arguments.first().map(String::as_str);
    let mut redacted = Vec::with_capacity(arguments.len());
    let mut redact_next = false;
    for (index, argument) in arguments.iter().enumerate() {
        if redact_next {
            redacted.push("<redacted>".into());
            redact_next = false;
            continue;
        }
        if SENSITIVE_OPTIONS.contains(&argument.as_str()) {
            redacted.push(argument.clone());
            redact_next = true;
            continue;
        }
        if let Some(option) = SENSITIVE_OPTIONS
            .iter()
            .find(|option| argument.starts_with(&format!("{option}=")))
        {
            redacted.push(format!("{option}=<redacted>"));
            continue;
        }
        if argument == "--env" {
            redacted.push(argument.clone());
            redact_next = true;
            continue;
        }
        if argument.starts_with("--env=") {
            redacted.push("--env=<redacted>".into());
            continue;
        }
        if command == Some("httpproxy") && index == 4 && !argument.starts_with('-') {
            redacted.push("<redacted>".into());
            continue;
        }
        redacted.push(argument.clone());
    }
    redacted
}

#[cfg(test)]
mod tests {
    use rust_ios_protocol::OutputOptions;

    use crate::MockDeviceBackend;

    use super::*;

    #[tokio::test]
    async fn records_commands_but_redacts_pair_material() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.jsonl");
        let mock = MockDeviceBackend::new();
        mock.attach_usb("phone").await;
        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(
            mock.start_userspace_tunnel(&TunnelSpec {
                udid: Some("phone".into()),
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
        let backend = RecordingBackend::new(Arc::new(mock), &path);
        backend.read_pair_record("phone").await.unwrap();
        backend
            .execute(&CommandInvocation {
                argv: vec!["info".into()],
                cwd: None,
                udid: Some("phone".into()),
                output: OutputOptions::default(),
            })
            .await
            .unwrap();
        backend
            .execute(&CommandInvocation {
                argv: vec![
                    "wifi".into(),
                    "--ssid=Lab".into(),
                    "--password".into(),
                    "wifi-secret".into(),
                ],
                cwd: None,
                udid: Some("phone".into()),
                output: OutputOptions::default(),
            })
            .await
            .unwrap();
        backend
            .execute(&CommandInvocation {
                argv: vec![
                    "httpproxy".into(),
                    "proxy.example".into(),
                    "8080".into(),
                    "user".into(),
                    "proxy-secret".into(),
                    "--p12file=identity.p12".into(),
                    "--password=p12-secret".into(),
                ],
                cwd: None,
                udid: Some("phone".into()),
                output: OutputOptions::default(),
            })
            .await
            .unwrap();
        backend
            .execute_with_tunnel(
                &CommandInvocation {
                    argv: vec!["ps".into()],
                    cwd: None,
                    udid: Some("phone".into()),
                    output: OutputOptions::default(),
                },
                Some(tunnel),
            )
            .await
            .unwrap();
        let mut location = backend
            .start_location_simulation(
                &LocationSimulationSpec {
                    udid: Some("phone".into()),
                    source_path: "C:/private/routes/recording-secret-track.gpx".into(),
                    points: vec![rust_ios_protocol::LocationSimulationPoint {
                        latitude: 12.345_678,
                        longitude: -98.765_432,
                        delay_before_ms: 1_234,
                        source_time: Some("2026-07-16T12:34:56.789Z".into()),
                    }],
                    output_path: Some("C:/private/location-output.jsonl".into()),
                },
                None,
            )
            .await
            .unwrap();
        location.close().await.unwrap();
        let text = tokio::fs::read_to_string(path).await.unwrap();
        assert!(text.contains("read_pair_record"));
        assert!(text.contains("redacted"));
        assert!(!text.contains("mock-host"));
        assert!(text.contains("ProductVersion"));
        assert!(text.contains("<redacted>"));
        assert!(!text.contains("wifi-secret"));
        assert!(!text.contains("proxy-secret"));
        assert!(!text.contains("p12-secret"));
        assert!(text.contains("execute_with_tunnel"));
        assert!(text.contains("\"sharedRsd\":true"));
        assert!(text.contains("start_location_simulation"));
        assert!(text.contains("\"pointCount\":1"));
        assert!(text.contains("\"locationDataRedacted\":true"));
        assert!(!text.contains("recording-secret-track.gpx"));
        assert!(!text.contains("location-output.jsonl"));
        assert!(!text.contains("12.345678"));
        assert!(!text.contains("-98.765432"));
        assert!(!text.contains("2026-07-16T12:34:56.789Z"));
    }
}
