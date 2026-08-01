use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult,
    ConnectionPreference, DeviceInfo, DeviceList, DeviceStateSpec, LocationSimulationSpec,
    RemotePairingInfo, StreamTaskSpec, TunnelPairSpec, TunnelSpec, UiRunnerSpec, WdaSpec,
    WebInspectorCdpSpec, WebInspectorJsShellSpec, XCTestEvent, XCTestRunSummary, XCTestSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::CoreResult;

pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send + Sync + Debug {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send + Sync + Debug {}
pub type BoxedIo = Box<dyn AsyncReadWrite>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTarget {
    pub udid: Option<String>,
    pub connection: ConnectionPreference,
}

impl Default for DeviceTarget {
    fn default() -> Self {
        Self {
            udid: None,
            connection: ConnectionPreference::UsbPreferred,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserSpaceTunnelInfo {
    pub udid: String,
    pub client_address: String,
    pub server_address: String,
    pub netmask: String,
    pub mtu: u16,
    pub rsd_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RsdServiceInfo {
    #[serde(rename = "Port")]
    pub port: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsdServiceKind {
    DebugProxy,
    Dvt,
    AppService,
    OpenStdio,
    WebInspector,
    DeviceInfo,
    FileServiceControl,
    FileServiceData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTransport {
    Lockdown,
    SharedRsd,
    DedicatedCoreDevice,
    Mock,
}

impl RsdServiceKind {
    pub(crate) fn service_name(self) -> &'static str {
        match self {
            Self::DebugProxy => "com.apple.internal.dt.remote.debugproxy",
            Self::Dvt => "com.apple.instruments.dtservicehub",
            Self::AppService => "com.apple.coredevice.appservice",
            Self::OpenStdio => "com.apple.coredevice.openstdiosocket",
            Self::WebInspector => "com.apple.webinspector.shim.remote",
            Self::DeviceInfo => "com.apple.coredevice.deviceinfo",
            Self::FileServiceControl => "com.apple.coredevice.fileservice.control",
            Self::FileServiceData => "com.apple.coredevice.fileservice.data",
        }
    }
}

#[async_trait]
pub trait UserSpaceTunnel: Debug + Send + Sync {
    fn info(&self) -> &UserSpaceTunnelInfo;
    async fn connect(&self, port: u16) -> CoreResult<BoxedIo>;
    async fn rsd_services(&self) -> CoreResult<BTreeMap<String, RsdServiceInfo>>;
    async fn ensure_rsd_service(&self, service: RsdServiceKind) -> CoreResult<()> {
        let name = service.service_name();
        if self.rsd_services().await?.contains_key(name) {
            Ok(())
        } else {
            Err(crate::CoreError::DeviceUnavailable(format!(
                "RSD service {name:?} is not advertised"
            )))
        }
    }
    async fn connect_rsd_service(&self, service: RsdServiceKind) -> CoreResult<BoxedIo>;
    async fn close(&self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelTunnelInfo {
    pub udid: String,
    pub interface_name: String,
    pub client_address: String,
    pub server_address: String,
    pub netmask: String,
    pub mtu: u16,
    pub rsd_port: u16,
}

#[async_trait]
pub trait KernelTunnelRunner: Debug + Send {
    async fn run(&mut self, cancel: CancellationToken) -> CoreResult<()>;
    async fn close(&mut self) -> CoreResult<()>;
}

pub struct KernelTunnelHandle {
    pub info: KernelTunnelInfo,
    pub route: Arc<dyn UserSpaceTunnel>,
    pub runner: Box<dyn KernelTunnelRunner>,
}

impl Debug for KernelTunnelHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KernelTunnelHandle")
            .field("info", &self.info)
            .field("route", &self.route)
            .field("runner", &self.runner)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceStreamInfo {
    pub content_type: String,
    #[serde(default)]
    pub initial_bytes: Vec<u8>,
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait DeviceStream: Debug + Send {
    fn info(&self) -> &DeviceStreamInfo;
    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceConditionInfo {
    pub udid: String,
    pub condition_identifier: String,
    pub profile_identifier: String,
    pub profile_description: String,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait DeviceCondition: Debug + Send + Sync {
    fn info(&self) -> &DeviceConditionInfo;
    async fn close(&self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocationSimulationInfo {
    pub udid: String,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait DeviceLocationSimulation: Debug + Send {
    fn info(&self) -> &LocationSimulationInfo;
    async fn set_location(&mut self, latitude: f64, longitude: f64) -> CoreResult<()>;
    async fn clear(&mut self) -> CoreResult<()>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WdaSessionInfo {
    pub udid: String,
    pub wda_url: String,
    pub mjpeg_url: String,
    pub device_http_port: u16,
    pub device_mjpeg_port: u16,
    pub status: serde_json::Value,
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait WdaSession: Debug + Send {
    fn info(&self) -> &WdaSessionInfo;
    /// 非阻塞读取一条 XCTest event，用于 server-owned WDA 日志。
    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>>;
    async fn status(&mut self) -> CoreResult<serde_json::Value>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiRunnerSessionInfo {
    pub udid: String,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait UiRunnerSession: Debug + Send {
    fn info(&self) -> &UiRunnerSessionInfo;
    /// 检查 runner 是否仍在运行；runner 退出时返回具体错误。
    async fn ensure_running(&mut self) -> CoreResult<()>;
    /// 非阻塞读取一条 XCTest event，用于 server-owned 日志。
    fn try_next_event(&mut self) -> CoreResult<Option<XCTestEvent>>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct XCTestSessionInfo {
    pub udid: String,
    pub target_count: usize,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[derive(Debug, Clone, PartialEq)]
pub enum XCTestSessionUpdate {
    Event(XCTestEvent),
    Finished(XCTestRunSummary),
}

#[async_trait]
pub trait XCTestSession: Debug + Send {
    fn info(&self) -> &XCTestSessionInfo;
    async fn next_update(&mut self) -> CoreResult<XCTestSessionUpdate>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppProcessSessionInfo {
    pub udid: String,
    pub bundle_id: String,
    pub pid: u32,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait AppProcessSession: Debug + Send {
    fn info(&self) -> &AppProcessSessionInfo;
    /// 返回下一段合并后的 stdout/stderr；`None` 表示进程输出流已到 EOF。
    async fn next_chunk(&mut self) -> CoreResult<Option<Vec<u8>>>;
    /// 结束由本 session 启动且仍在运行的进程。
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebInspectorPageInfo {
    pub key: String,
    pub page_id: u64,
    pub application_id: String,
    pub bundle_id: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebInspectorCdpSessionInfo {
    pub udid: String,
    pub pages: Vec<WebInspectorPageInfo>,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait WebInspectorCdpSession: Debug + Send {
    fn info(&self) -> &WebInspectorCdpSessionInfo;
    async fn refresh_pages(&mut self) -> CoreResult<Vec<WebInspectorPageInfo>>;
    async fn attach_page(&mut self, page_key: &str) -> CoreResult<()>;
    async fn send_message(&mut self, message: Value) -> CoreResult<()>;
    async fn next_message(&mut self, timeout: Duration) -> CoreResult<Option<Value>>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebInspectorJsShellSessionInfo {
    pub udid: String,
    pub page: WebInspectorPageInfo,
    #[serde(rename = "serviceTransport")]
    pub service_transport: ServiceTransport,
}

#[async_trait]
pub trait WebInspectorJsShellSession: Debug + Send {
    fn info(&self) -> &WebInspectorJsShellSessionInfo;
    async fn evaluate(&mut self, expression: &str) -> CoreResult<Value>;
    async fn close(&mut self) -> CoreResult<()>;
}

#[async_trait]
pub trait DeviceBackend: Debug + Send + Sync {
    fn name(&self) -> &'static str;

    async fn list_devices(&self) -> CoreResult<DeviceList>;
    async fn read_buid(&self) -> CoreResult<String>;
    async fn read_pair_record(&self, udid: &str) -> CoreResult<Value>;
    async fn connect_port(&self, target: &DeviceTarget, port: u16) -> CoreResult<BoxedIo>;
    async fn start_userspace_tunnel(
        &self,
        spec: &TunnelSpec,
    ) -> CoreResult<Box<dyn UserSpaceTunnel>>;
    async fn start_kernel_tunnel(&self, spec: &TunnelSpec) -> CoreResult<KernelTunnelHandle>;
    async fn pair_remote_tunnel(&self, spec: &TunnelPairSpec) -> CoreResult<RemotePairingInfo>;
    async fn start_stream(
        &self,
        spec: &StreamTaskSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceStream>>;
    async fn start_accessibility_inspector(
        &self,
        spec: &AccessibilityInspectorSpec,
    ) -> CoreResult<Box<dyn DeviceStream>>;
    async fn start_device_condition(
        &self,
        spec: &DeviceStateSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceCondition>>;
    async fn start_location_simulation(
        &self,
        spec: &LocationSimulationSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn DeviceLocationSimulation>>;
    async fn start_wda_session(
        &self,
        spec: &WdaSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WdaSession>>;
    async fn start_ui_runner_session(
        &self,
        spec: &UiRunnerSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn UiRunnerSession>>;
    async fn start_xctest_session(
        &self,
        spec: &XCTestSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn XCTestSession>>;
    async fn start_app_process_session(
        &self,
        spec: &AppLaunchSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn AppProcessSession>>;
    async fn start_webinspector_cdp_session(
        &self,
        spec: &WebInspectorCdpSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorCdpSession>>;
    async fn start_webinspector_js_shell_session(
        &self,
        spec: &WebInspectorJsShellSpec,
        tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<Box<dyn WebInspectorJsShellSession>>;

    async fn execute(&self, invocation: &CommandInvocation) -> CoreResult<CommandResult> {
        let command = invocation
            .argv
            .first()
            .map(String::as_str)
            .unwrap_or("<empty>");
        Err(crate::CoreError::NotImplemented(format!(
            "command {command:?} is registered but has no backend implementation"
        )))
    }

    async fn execute_with_tunnel(
        &self,
        invocation: &CommandInvocation,
        _tunnel: Option<Arc<dyn UserSpaceTunnel>>,
    ) -> CoreResult<CommandResult> {
        self.execute(invocation).await
    }

    async fn resolve_device(&self, target: &DeviceTarget) -> CoreResult<DeviceInfo> {
        let devices = self.list_devices().await?.devices;
        select_device(devices, target)
    }
}

pub fn select_device(devices: Vec<DeviceInfo>, target: &DeviceTarget) -> CoreResult<DeviceInfo> {
    let mut matching: Vec<DeviceInfo> = devices
        .into_iter()
        .filter(|device| target.udid.as_ref().is_none_or(|udid| device.udid == *udid))
        .filter(|device| match target.connection {
            ConnectionPreference::UsbOnly => {
                device.connection_type == rust_ios_protocol::ConnectionType::Usb
            }
            ConnectionPreference::NetworkOnly => {
                device.connection_type == rust_ios_protocol::ConnectionType::Network
            }
            ConnectionPreference::UsbPreferred | ConnectionPreference::Any => true,
        })
        .collect();

    if matching.is_empty() {
        return Err(crate::CoreError::DeviceNotFound(
            target
                .udid
                .clone()
                .unwrap_or_else(|| "no connected device matches the selector".into()),
        ));
    }

    if target.connection == ConnectionPreference::UsbPreferred
        && let Some(index) = matching
            .iter()
            .position(|device| device.connection_type == rust_ios_protocol::ConnectionType::Usb)
    {
        return Ok(matching.swap_remove(index));
    }

    Ok(matching.swap_remove(0))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rust_ios_protocol::{ConnectionType, DeviceInfo};

    use super::*;

    fn device(udid: &str, connection_type: ConnectionType) -> DeviceInfo {
        DeviceInfo {
            device_id: 1,
            udid: udid.into(),
            connection_type,
            network_address: None,
            properties: BTreeMap::new(),
        }
    }

    #[test]
    fn usb_preferred_selects_usb_even_if_network_is_first() {
        let selected = select_device(
            vec![
                device("same", ConnectionType::Network),
                device("same", ConnectionType::Usb),
            ],
            &DeviceTarget {
                udid: Some("same".into()),
                connection: ConnectionPreference::UsbPreferred,
            },
        )
        .unwrap();
        assert_eq!(selected.connection_type, ConnectionType::Usb);
    }

    #[test]
    fn explicit_connection_filter_is_enforced() {
        let error = select_device(
            vec![device("phone", ConnectionType::Network)],
            &DeviceTarget {
                udid: Some("phone".into()),
                connection: ConnectionPreference::UsbOnly,
            },
        )
        .unwrap_err();
        assert!(matches!(error, crate::CoreError::DeviceNotFound(_)));
    }
}
