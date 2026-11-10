use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello(ClientHello),
    Request(RequestEnvelope),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerMessage {
    Hello(ServerHello),
    Response(ResponseEnvelope),
    Event(EventEnvelope),
    Fatal(ProtocolError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub client_version: String,
    pub client_pid: u32,
    pub auth_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerHello {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub server_version: String,
    pub server_pid: u32,
    pub instance_id: Uuid,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestEnvelope {
    pub request_id: Uuid,
    pub request: Request,
}

impl RequestEnvelope {
    pub fn new(request: Request) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            request,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseEnvelope {
    pub request_id: Uuid,
    pub response: ResponseBody,
}

impl ResponseEnvelope {
    pub fn ok(request_id: Uuid, payload: ResponsePayload) -> Self {
        Self {
            request_id,
            response: ResponseBody::Ok(payload),
        }
    }

    pub fn error(request_id: Uuid, error: ProtocolError) -> Self {
        Self {
            request_id,
            response: ResponseBody::Error(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ResponseBody {
    Ok(ResponsePayload),
    Error(ProtocolError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", content = "arguments", rename_all = "snake_case")]
pub enum Request {
    Ping,
    ServerStatus,
    Shutdown,
    ListDevices {
        details: bool,
    },
    ListenSnapshot,
    WaitDeviceChange {
        previous: DeviceList,
        timeout_ms: u32,
    },
    ReadBuid,
    ReadPairRecord {
        udid: String,
    },
    TaskList {
        kind: Option<TaskKind>,
    },
    TaskShow {
        task_id: Uuid,
    },
    TaskStop {
        selector: TaskSelector,
    },
    TaskRestart {
        task_id: Uuid,
    },
    TaskOutput {
        task_id: Uuid,
        offset: u64,
        limit: u32,
    },
    ForwardStart {
        specs: Vec<ForwardSpec>,
    },
    TunnelStart {
        spec: TunnelSpec,
    },
    TunnelPair {
        spec: TunnelPairSpec,
    },
    TunnelRefresh {
        udid: Option<String>,
    },
    StreamStart {
        spec: StreamTaskSpec,
    },
    AccessibilityInspectorStart {
        spec: AccessibilityInspectorSpec,
    },
    LocationSimulationStart {
        spec: LocationSimulationSpec,
    },
    ScreenshotStreamStart {
        spec: ScreenshotStreamSpec,
    },
    UiStreamStart {
        spec: UiStreamSpec,
    },
    UiRunnerStart {
        spec: UiRunnerSpec,
    },
    WebInspectorCdpStart {
        spec: WebInspectorCdpSpec,
    },
    WebInspectorJsShellStart {
        spec: WebInspectorJsShellSpec,
    },
    WebInspectorJsShellEval {
        task_id: Uuid,
        expression: String,
    },
    DebugProxyStart {
        spec: DebugProxySpec,
    },
    DproxyStart {
        spec: DproxySpec,
    },
    WdaStart {
        spec: WdaSpec,
    },
    #[serde(rename = "xctest_start")]
    XCTestStart {
        spec: XCTestSpec,
    },
    AppLaunchStart {
        spec: AppLaunchSpec,
    },
    Execute {
        invocation: CommandInvocation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
// Task 变体携带 TaskRecord（含 TaskSpec），规格字段增长会放大变体尺寸差；
// 该枚举只经 IPC 序列化传输，栈上尺寸差无害（serde 按字段编码，不拷贝整体）。
#[allow(clippy::large_enum_variant)]
pub enum ResponsePayload {
    Pong { server_time_ms: u64 },
    Ack { message: String },
    ServerStatus(ServerStatus),
    Devices(DeviceList),
    Buid { buid: String },
    PairRecord { udid: String, record: Value },
    Tasks { tasks: Vec<TaskRecord> },
    Task { task: TaskRecord },
    TaskOutput(TaskOutput),
    Command(CommandResult),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventEnvelope {
    pub subscription_id: Uuid,
    pub sequence: u64,
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum Event {
    DeviceSnapshot(DeviceList),
    TaskChanged(Box<TaskRecord>),
    Output(CommandOutputChunk),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthenticationFailed,
    ProtocolMismatch,
    InvalidRequest,
    ServerUnavailable,
    AlreadyRunning,
    NotFound,
    Conflict,
    DeviceNotFound,
    DeviceUnavailable,
    UsbmuxdUnavailable,
    PairingRequired,
    // 设备处于锁定状态（passcode screen），配对被推迟；解锁后重试即可。
    // 与 PairingRequired 的区别是它已到达设备信任链路，只是无法持久化
    // pair record（go-ios v1.2.1 ExitCodeDeviceLocked=2）。
    DeviceLocked,
    PermissionDenied,
    UnsupportedPlatform,
    NotImplemented,
    Timeout,
    TaskFailed,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerStatus {
    pub version: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub pid: u32,
    pub instance_id: Uuid,
    pub started_at_ms: u64,
    pub uptime_ms: u64,
    pub listen_addr: String,
    pub data_dir: String,
    pub device_count: usize,
    pub task_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DeviceList {
    #[serde(rename = "deviceList")]
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_id: u32,
    pub udid: String,
    pub connection_type: ConnectionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_address: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Usb,
    Network,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Forward,
    Tunnel,
    Syslog,
    OsTrace,
    Pcap,
    Notifications,
    Sysmontap,
    /// CoreDevice display 媒体流（同步 go-ios `7e82aa2`；仅内核隧道，UDP 接收）。
    DisplayStream,
    AccessibilityInspector,
    LocationSimulation,
    DeviceState,
    ScreenshotStream,
    UiStream,
    WebInspectorCdp,
    WebInspectorJsShell,
    UiRunner,
    DebugProxy,
    Dproxy,
    Wda,
    #[serde(rename = "xctest")]
    XCTest,
    AppProcess,
    Other,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Tunnel => "tunnel",
            Self::Syslog => "syslog",
            Self::OsTrace => "os_trace",
            Self::Pcap => "pcap",
            Self::Notifications => "notifications",
            Self::Sysmontap => "sysmontap",
            Self::DisplayStream => "display_stream",
            Self::AccessibilityInspector => "accessibility_inspector",
            Self::LocationSimulation => "location_simulation",
            Self::DeviceState => "device_state",
            Self::ScreenshotStream => "screenshot_stream",
            Self::UiStream => "ui_stream",
            Self::WebInspectorCdp => "webinspector_cdp",
            Self::WebInspectorJsShell => "webinspector_js_shell",
            Self::UiRunner => "ui_runner",
            Self::DebugProxy => "debug_proxy",
            Self::Dproxy => "dproxy",
            Self::Wda => "wda",
            Self::XCTest => "xctest",
            Self::AppProcess => "app_process",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Starting,
    Waiting,
    Running,
    BackingOff,
    Stopping,
    Stopped,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRecord {
    pub id: Uuid,
    pub kind: TaskKind,
    pub desired_state: DesiredState,
    pub runtime_state: RuntimeState,
    pub restart_policy: RestartPolicy,
    pub generation: u64,
    pub attempts: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub spec: TaskSpec,
    #[serde(default)]
    pub runtime: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "configuration", rename_all = "snake_case")]
pub enum TaskSpec {
    Forward(ForwardSpec),
    Tunnel(TunnelSpec),
    Stream(StreamTaskSpec),
    AccessibilityInspector(AccessibilityInspectorSpec),
    LocationSimulation(LocationSimulationSpec),
    ScreenshotStream(ScreenshotStreamSpec),
    UiStream(UiStreamSpec),
    UiRunner(Box<UiRunnerSpec>),
    WebInspectorCdp(WebInspectorCdpSpec),
    WebInspectorJsShell(WebInspectorJsShellSpec),
    DebugProxy(DebugProxySpec),
    Dproxy(DproxySpec),
    Wda(Box<WdaSpec>),
    #[serde(rename = "xctest")]
    XCTest(Box<XCTestSpec>),
    AppLaunch(AppLaunchSpec),
    DeviceState(DeviceStateSpec),
    Generic(GenericTaskSpec),
}

impl TaskSpec {
    pub fn kind(&self) -> TaskKind {
        match self {
            Self::Forward(_) => TaskKind::Forward,
            Self::Tunnel(_) => TaskKind::Tunnel,
            Self::Stream(spec) => spec.kind.clone(),
            Self::AccessibilityInspector(_) => TaskKind::AccessibilityInspector,
            Self::LocationSimulation(_) => TaskKind::LocationSimulation,
            Self::ScreenshotStream(_) => TaskKind::ScreenshotStream,
            Self::UiStream(_) => TaskKind::UiStream,
            Self::UiRunner(_) => TaskKind::UiRunner,
            Self::WebInspectorCdp(_) => TaskKind::WebInspectorCdp,
            Self::WebInspectorJsShell(_) => TaskKind::WebInspectorJsShell,
            Self::DebugProxy(_) => TaskKind::DebugProxy,
            Self::Dproxy(_) => TaskKind::Dproxy,
            Self::Wda(_) => TaskKind::Wda,
            Self::XCTest(_) => TaskKind::XCTest,
            Self::AppLaunch(_) => TaskKind::AppProcess,
            Self::DeviceState(_) => TaskKind::DeviceState,
            Self::Generic(spec) => spec.kind.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForwardSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub bind_host: String,
    pub host_port: u16,
    pub device_port: u16,
    #[serde(default)]
    pub connection: ConnectionPreference,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPreference {
    #[default]
    UsbPreferred,
    UsbOnly,
    NetworkOnly,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default)]
    pub mode: TunnelMode,
    #[serde(default)]
    pub transport: TunnelTransport,
    pub pair_record_path: String,
    pub userspace_bind_host: String,
    pub userspace_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_interface_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wintun_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelMode {
    #[default]
    Userspace,
    Kernel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelTransport {
    #[default]
    UsbCoreDevice,
    RemotePairingTcp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelPairSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub pair_record_path: String,
    #[serde(default)]
    pub manual: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotePairingInfo {
    pub udid: String,
    pub identity_path: String,
    pub device_address: String,
    pub newly_paired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamTaskSpec {
    pub kind: TaskKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(default)]
    pub format: StreamFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_trace: Option<OsTraceOptions>,
    /// `kill --watch` 目标（同步 go-ios `5f16b28` WatchKill）：通知流匹配到目标
    /// 进入 Running 状态时，在同一条 DVT 连接的 processControl 通道上秒杀。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_kill_targets: Option<Vec<String>>,
    /// display 媒体流要镜像的显示器 ID（同步 go-ios `7e82aa2`；None = 主屏）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i64>,
    /// display 媒体流的宿主 UDP 接收端口（内核隧道接口上）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OsTraceOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<OsTraceLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_excludes: Option<String>,
    #[serde(default)]
    pub follow: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OsTraceLevel {
    Default,
    Info,
    Debug,
    Error,
    Fault,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessibilityInspectorSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// go-ios `ax --font` parity. This changes the device DYNAMIC_TYPE setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScreenshotStreamSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub bind_host: String,
    pub port: u16,
    #[serde(default = "default_screenshot_interval_ms")]
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiStreamSpec {
    pub driver: UiAutomationDriver,
    pub format: UiStreamFormat,
    pub wda_url: String,
    pub devicekit_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wda_task_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiRunnerSpec {
    pub target: UiRunnerTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub runner_bundle_id: String,
    pub target_bundle_id: String,
    pub xctest_config: String,
    pub bind_host: String,
    pub host_port: u16,
    pub device_port: u16,
    pub health_path: String,
    #[serde(default = "default_ui_runner_readiness_timeout_ms")]
    pub readiness_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiRunnerTarget {
    Wda,
    DeviceKit,
}

impl UiRunnerTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wda => "wda",
            Self::DeviceKit => "devicekit",
        }
    }
}

const fn default_ui_runner_readiness_timeout_ms() -> u64 {
    120_000
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiAutomationDriver {
    Wda,
    DeviceKit,
    Auto,
}

impl UiAutomationDriver {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wda => "wda",
            Self::DeviceKit => "devicekit",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiStreamFormat {
    Mjpeg,
    H264,
}

impl UiStreamFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mjpeg => "mjpeg",
            Self::H264 => "h264",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebInspectorCdpSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub bind_host: String,
    pub port: u16,
    #[serde(default = "default_webinspector_timeout_ms")]
    pub startup_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebInspectorJsShellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub open_safari: bool,
    #[serde(default = "default_webinspector_timeout_ms")]
    pub command_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

const fn default_webinspector_timeout_ms() -> u64 {
    15_000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DebugProxySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub bind_host: String,
    pub port: u16,
    #[serde(default)]
    pub client_owned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DproxySpec {
    pub bind_host: String,
    pub port: u16,
    #[serde(default = "default_dproxy_upstream")]
    pub upstream_address: String,
    #[serde(default)]
    pub binary_only: bool,
    #[serde(default)]
    pub mode: DproxyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

fn default_dproxy_upstream() -> String {
    "127.0.0.1:27015".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DproxyMode {
    #[default]
    All,
    Usbmuxd,
    Utun,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DebugAppInfo {
    pub bundle_id: String,
    pub local_app_path: String,
    pub local_executable_path: String,
    pub device_app_path: String,
    pub device_executable_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WdaSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub runner_bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xctest_config: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    #[serde(default = "default_wda_readiness_timeout_ms")]
    pub readiness_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct XCTestSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub targets: Vec<XCTestTargetSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    /// JUnit XML 输出路径（同步 go-ios `runtest --junit-output`，aeda162）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junit_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_xctestrun_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppLaunchSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub bundle_id: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub kill_existing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct XCTestTargetSpec {
    pub name: String,
    pub runner_bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_app_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xctest_config: Option<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, XCTestEnvironmentValue>,
    #[serde(default)]
    pub tests_to_run: Vec<String>,
    #[serde(default)]
    pub tests_to_skip: Vec<String>,
    pub is_ui_test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum XCTestEnvironmentValue {
    String(String),
    Boolean(bool),
    SignedInteger(i64),
    UnsignedInteger(u64),
    Real(f64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct XCTestEvent {
    pub timestamp_ms: u64,
    pub target_index: usize,
    pub target_name: String,
    pub event: XCTestEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub enum XCTestEventKind {
    TargetStarted,
    TargetFinished {
        outcome: String,
    },
    TestPlanStarted,
    TestPlanFinished,
    SuiteStarted {
        suite: String,
        started_at: String,
    },
    SuiteFinished {
        suite: String,
        finished_at: String,
        run_count: u64,
        failures: u64,
        unexpected: u64,
        test_duration: f64,
        total_duration: f64,
        skipped: u64,
        expected_failures: u64,
        uncaught_exceptions: u64,
    },
    CaseStarted {
        test_class: String,
        method: String,
    },
    CaseFinished {
        test_class: String,
        method: String,
        status: String,
        duration: f64,
    },
    CaseFailed {
        test_class: String,
        method: String,
        message: String,
        file: String,
        line: u64,
    },
    CaseStalled {
        test_class: String,
        method: String,
        file: String,
        line: u64,
    },
    ActivityStarted {
        test_class: String,
        method: String,
        title: String,
    },
    ActivityFinished {
        test_class: String,
        method: String,
        title: String,
    },
    Metric {
        test_class: String,
        method: String,
        metric: String,
        file: String,
        line: u64,
    },
    Log {
        message: String,
        debug: bool,
    },
    ProtocolVersion {
        current: u64,
        minimum: u64,
    },
    UiInitializationStarted,
    UiInitializationFailed {
        description: String,
    },
    BootstrapFailed {
        description: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct XCTestRunSummary {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub duration_ms: u64,
    pub target_count: usize,
    pub completed_targets: usize,
    pub suite_count: u64,
    pub test_count: u64,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub expected_failures: u64,
    pub stalled: u64,
}

fn default_wda_readiness_timeout_ms() -> u64 {
    30_000
}

fn default_screenshot_interval_ms() -> u64 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceStateSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub condition_identifier: String,
    pub profile_identifier: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationSimulationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    pub source_path: String,
    pub points: Vec<LocationSimulationPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocationSimulationPoint {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub delay_before_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum StreamFormat {
    #[default]
    Raw,
    JsonLines,
    Pcap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskOutput {
    pub task_id: Uuid,
    pub offset: u64,
    pub next_offset: u64,
    pub total_bytes: u64,
    pub eof: bool,
    pub content_type: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenericTaskSpec {
    pub kind: TaskKind,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "selector", content = "value", rename_all = "snake_case")]
pub enum TaskSelector {
    Id(Uuid),
    ForwardPort(u16),
    TunnelUdid(String),
    Kind(TaskKind),
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandInvocation {
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default)]
    pub output: OutputOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OutputOptions {
    pub no_json: bool,
    pub pretty: bool,
    pub verbose: bool,
    pub trace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandResult {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandOutputChunk {
    pub stream: OutputStream,
    pub bytes_base64: String,
    pub eof: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
    Binary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trip_has_stable_tags() {
        let request = RequestEnvelope::new(Request::ListDevices { details: true });
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["request"]["command"], "list_devices");
        assert_eq!(json["request"]["arguments"]["details"], true);
        let decoded: RequestEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn legacy_tunnel_spec_defaults_to_usb_core_device() {
        let spec: TunnelSpec = serde_json::from_value(serde_json::json!({
            "udid": "phone",
            "mode": "userspace",
            "pair_record_path": ".",
            "userspace_bind_host": "127.0.0.1",
            "userspace_port": 0
        }))
        .unwrap();
        assert_eq!(spec.transport, TunnelTransport::UsbCoreDevice);
        assert_eq!(spec.kernel_interface_name, None);
        assert_eq!(spec.wintun_path, None);
    }

    #[test]
    fn kernel_tunnel_options_have_stable_wire_names() {
        let spec = TunnelSpec {
            udid: Some("phone".into()),
            mode: TunnelMode::Kernel,
            transport: TunnelTransport::UsbCoreDevice,
            pair_record_path: "default".into(),
            userspace_bind_host: "127.0.0.1".into(),
            userspace_port: 0,
            kernel_interface_name: Some("rust-ios-phone".into()),
            wintun_path: Some("C:\\Windows\\System32\\wintun.dll".into()),
        };
        let encoded = serde_json::to_value(&spec).unwrap();
        assert_eq!(encoded["kernel_interface_name"], "rust-ios-phone");
        assert_eq!(encoded["wintun_path"], "C:\\Windows\\System32\\wintun.dll");
        assert_eq!(serde_json::from_value::<TunnelSpec>(encoded).unwrap(), spec);
    }

    #[test]
    fn dproxy_wire_tags_and_legacy_defaults_are_stable() {
        let request = Request::DproxyStart {
            spec: DproxySpec {
                bind_host: "127.0.0.1".into(),
                port: 43210,
                upstream_address: "127.0.0.1:27015".into(),
                binary_only: true,
                mode: DproxyMode::Usbmuxd,
                interface: None,
                output_path: Some("C:/captures/dproxy.ndjson".into()),
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "dproxy_start");
        assert_eq!(encoded["arguments"]["spec"]["mode"], "usbmuxd");
        assert_eq!(encoded["arguments"]["spec"]["binary_only"], true);
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let legacy: DproxySpec = serde_json::from_value(serde_json::json!({
            "bind_host": "127.0.0.1",
            "port": 0
        }))
        .unwrap();
        assert_eq!(legacy.upstream_address, "127.0.0.1:27015");
        assert_eq!(legacy.mode, DproxyMode::All);
        assert!(!legacy.binary_only);
        assert_eq!(legacy.interface, None);
        assert_eq!(legacy.output_path, None);

        let task = TaskSpec::Dproxy(legacy);
        assert_eq!(task.kind(), TaskKind::Dproxy);
        let encoded = serde_json::to_value(&task).unwrap();
        assert_eq!(encoded["kind"], "dproxy");
        assert_eq!(serde_json::from_value::<TaskSpec>(encoded).unwrap(), task);
    }

    #[test]
    fn legacy_stream_spec_defaults_to_no_ostrace_options() {
        let spec: StreamTaskSpec = serde_json::from_value(serde_json::json!({
            "kind": "os_trace",
            "format": "json_lines",
            "pid": 42
        }))
        .unwrap();
        assert_eq!(spec.os_trace, None);
    }

    #[test]
    fn ostrace_options_have_stable_typed_fields() {
        let spec = TaskSpec::Stream(StreamTaskSpec {
            kind: TaskKind::OsTrace,
            udid: Some("phone".into()),
            output_path: Some("C:/captures/ostrace.jsonl".into()),
            format: StreamFormat::JsonLines,
            pid: None,
            process: Some("Demo".into()),
            os_trace: Some(OsTraceOptions {
                levels: vec![OsTraceLevel::Error, OsTraceLevel::Fault],
                subsystem_contains: Some("com.example".into()),
                message_contains: Some("timeout".into()),
                message_excludes: Some("ignored".into()),
                follow: true,
            }),
            watch_kill_targets: None,
            display_id: None,
            receiver_port: None,
        });
        let encoded = serde_json::to_value(spec).unwrap();
        assert_eq!(encoded["configuration"]["os_trace"]["levels"][0], "error");
        assert_eq!(
            encoded["configuration"]["os_trace"]["subsystem_contains"],
            "com.example"
        );
        assert_eq!(encoded["configuration"]["os_trace"]["follow"], true);
    }

    #[test]
    fn remote_pairing_request_has_stable_tag_without_persistent_pin() {
        let request = RequestEnvelope::new(Request::TunnelPair {
            spec: TunnelPairSpec {
                udid: Some("phone".into()),
                pair_record_path: "C:/isolated/remote-pairing".into(),
                manual: true,
            },
        });
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["request"]["command"], "tunnel_pair");
        assert_eq!(value["request"]["arguments"]["spec"]["manual"], true);
        assert!(value["request"]["arguments"]["spec"].get("pin").is_none());
    }

    #[test]
    fn task_spec_reports_kind() {
        let spec = TaskSpec::Forward(ForwardSpec {
            udid: None,
            bind_host: "127.0.0.1".into(),
            host_port: 8100,
            device_port: 8100,
            connection: ConnectionPreference::UsbPreferred,
        });
        assert_eq!(spec.kind(), TaskKind::Forward);

        let stream = TaskSpec::Stream(StreamTaskSpec {
            kind: TaskKind::Syslog,
            udid: None,
            output_path: None,
            format: StreamFormat::Raw,
            pid: None,
            process: None,
            os_trace: None,
            watch_kill_targets: None,
            display_id: None,
            receiver_port: None,
        });
        assert_eq!(stream.kind(), TaskKind::Syslog);

        let shell = TaskSpec::WebInspectorJsShell(WebInspectorJsShellSpec {
            udid: Some("test-phone".into()),
            url: None,
            bundle_id: Some("com.apple.mobilesafari".into()),
            open_safari: true,
            command_timeout_ms: 15_000,
            output_path: Some("C:/captures/js-shell.jsonl".into()),
        });
        assert_eq!(shell.kind(), TaskKind::WebInspectorJsShell);
        let encoded = serde_json::to_value(&shell).unwrap();
        assert_eq!(encoded["kind"], "web_inspector_js_shell");
        assert_eq!(serde_json::from_value::<TaskSpec>(encoded).unwrap(), shell);
        let eval = Request::WebInspectorJsShellEval {
            task_id: Uuid::nil(),
            expression: "2 + 2".into(),
        };
        let encoded = serde_json::to_value(&eval).unwrap();
        assert_eq!(encoded["command"], "web_inspector_js_shell_eval");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), eval);

        let accessibility = TaskSpec::AccessibilityInspector(AccessibilityInspectorSpec {
            udid: Some("test-phone".into()),
            output_path: Some("C:/captures/accessibility.jsonl".into()),
            font_size: Some(17.0),
        });
        assert_eq!(accessibility.kind(), TaskKind::AccessibilityInspector);
        let encoded = serde_json::to_value(&accessibility).unwrap();
        assert_eq!(encoded["kind"], "accessibility_inspector");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            accessibility
        );
        let request = Request::AccessibilityInspectorStart {
            spec: AccessibilityInspectorSpec {
                udid: Some("test-phone".into()),
                output_path: None,
                font_size: None,
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "accessibility_inspector_start");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let device_state = TaskSpec::DeviceState(DeviceStateSpec {
            udid: Some("test-phone".into()),
            condition_identifier: "SlowNetworkCondition".into(),
            profile_identifier: "SlowNetwork3GGood".into(),
        });
        assert_eq!(device_state.kind(), TaskKind::DeviceState);
        let encoded = serde_json::to_value(&device_state).unwrap();
        assert_eq!(encoded["kind"], "device_state");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            device_state
        );

        let location = TaskSpec::LocationSimulation(LocationSimulationSpec {
            udid: Some("test-phone".into()),
            source_path: "C:/tracks/demo.gpx".into(),
            points: vec![LocationSimulationPoint {
                latitude: 31.2304,
                longitude: 121.4737,
                delay_before_ms: 0,
                source_time: Some("2026-07-16T00:00:00Z".into()),
            }],
            output_path: None,
        });
        assert_eq!(location.kind(), TaskKind::LocationSimulation);
        let encoded = serde_json::to_value(&location).unwrap();
        assert_eq!(encoded["kind"], "location_simulation");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            location
        );
        let TaskSpec::LocationSimulation(location_spec) = location else {
            unreachable!();
        };
        let request = Request::LocationSimulationStart {
            spec: location_spec,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "location_simulation_start");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let screenshot = TaskSpec::ScreenshotStream(ScreenshotStreamSpec {
            udid: Some("test-phone".into()),
            bind_host: "127.0.0.1".into(),
            port: 3333,
            interval_ms: 100,
        });
        assert_eq!(screenshot.kind(), TaskKind::ScreenshotStream);
        let encoded = serde_json::to_value(&screenshot).unwrap();
        assert_eq!(encoded["kind"], "screenshot_stream");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            screenshot
        );

        let ui_stream = TaskSpec::UiStream(UiStreamSpec {
            driver: UiAutomationDriver::Wda,
            format: UiStreamFormat::Mjpeg,
            wda_url: "http://127.0.0.1:8100".into(),
            devicekit_url: "http://127.0.0.1:12004".into(),
            wda_task_id: None,
            output_path: Some("C:/captures/wda.mjpeg".into()),
            fps: Some(30),
            quality: Some(80),
            scale: Some(1.0),
            bitrate: None,
        });
        assert_eq!(ui_stream.kind(), TaskKind::UiStream);
        let encoded = serde_json::to_value(&ui_stream).unwrap();
        assert_eq!(encoded["kind"], "ui_stream");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            ui_stream
        );
        let TaskSpec::UiStream(ui_stream_spec) = ui_stream else {
            unreachable!();
        };
        let request = Request::UiStreamStart {
            spec: ui_stream_spec,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "ui_stream_start");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let ui_runner = TaskSpec::UiRunner(Box::new(UiRunnerSpec {
            target: UiRunnerTarget::DeviceKit,
            udid: Some("test-phone".into()),
            runner_bundle_id: "com.example.devicekit.runner".into(),
            target_bundle_id: "com.example.devicekit.runner".into(),
            xctest_config: "devicekit-iosUITests.xctest".into(),
            bind_host: "127.0.0.1".into(),
            host_port: 12004,
            device_port: 12004,
            health_path: "/health".into(),
            readiness_timeout_ms: 120_000,
            output_path: Some("C:/captures/ui-runner.jsonl".into()),
        }));
        assert_eq!(ui_runner.kind(), TaskKind::UiRunner);
        let encoded = serde_json::to_value(&ui_runner).unwrap();
        assert_eq!(encoded["kind"], "ui_runner");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            ui_runner
        );
        let TaskSpec::UiRunner(ui_runner_spec) = ui_runner else {
            unreachable!();
        };
        let request = Request::UiRunnerStart {
            spec: *ui_runner_spec,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "ui_runner_start");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let cdp = TaskSpec::WebInspectorCdp(WebInspectorCdpSpec {
            udid: Some("test-phone".into()),
            bind_host: "127.0.0.1".into(),
            port: 9222,
            startup_timeout_ms: 15_000,
        });
        assert_eq!(cdp.kind(), TaskKind::WebInspectorCdp);
        let encoded = serde_json::to_value(&cdp).unwrap();
        assert_eq!(encoded["kind"], "web_inspector_cdp");
        assert_eq!(serde_json::from_value::<TaskSpec>(encoded).unwrap(), cdp);

        let request = Request::WebInspectorCdpStart {
            spec: WebInspectorCdpSpec {
                udid: None,
                bind_host: "127.0.0.1".into(),
                port: 0,
                startup_timeout_ms: 15_000,
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["command"], "web_inspector_cdp_start");
        assert_eq!(serde_json::from_value::<Request>(encoded).unwrap(), request);

        let debug_proxy = TaskSpec::DebugProxy(DebugProxySpec {
            udid: Some("test-phone".into()),
            bind_host: "127.0.0.1".into(),
            port: 4444,
            client_owned: false,
        });
        assert_eq!(debug_proxy.kind(), TaskKind::DebugProxy);
        let encoded = serde_json::to_value(&debug_proxy).unwrap();
        assert_eq!(encoded["kind"], "debug_proxy");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            debug_proxy
        );
        let mut legacy = serde_json::to_value(&debug_proxy).unwrap();
        legacy["configuration"]
            .as_object_mut()
            .unwrap()
            .remove("client_owned");
        let TaskSpec::DebugProxy(legacy) = serde_json::from_value::<TaskSpec>(legacy).unwrap()
        else {
            unreachable!();
        };
        assert!(!legacy.client_owned);

        let wda = TaskSpec::Wda(Box::new(WdaSpec {
            udid: Some("test-phone".into()),
            runner_bundle_id: "com.example.WebDriverAgentRunner.xctrunner".into(),
            target_bundle_id: None,
            xctest_config: Some("WebDriverAgentRunner.xctest".into()),
            arguments: vec!["--disable-keyboard".into()],
            environment: BTreeMap::from([("USE_PORT".into(), "8100".into())]),
            output_path: Some("C:/captures/wda-events.jsonl".into()),
            readiness_timeout_ms: 15_000,
        }));
        assert_eq!(wda.kind(), TaskKind::Wda);
        let encoded = serde_json::to_value(&wda).unwrap();
        assert_eq!(encoded["kind"], "wda");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded.clone()).unwrap(),
            wda
        );
        let mut legacy = encoded;
        legacy["configuration"]
            .as_object_mut()
            .unwrap()
            .remove("output_path");
        let TaskSpec::Wda(legacy) = serde_json::from_value::<TaskSpec>(legacy).unwrap() else {
            unreachable!();
        };
        assert!(legacy.output_path.is_none());

        let xctest = TaskSpec::XCTest(Box::new(XCTestSpec {
            udid: Some("test-phone".into()),
            targets: vec![XCTestTargetSpec {
                name: "DemoUITests".into(),
                runner_bundle_id: "com.example.DemoUITests.xctrunner".into(),
                target_bundle_id: Some("com.example.Demo".into()),
                target_app_name: None,
                xctest_config: Some("DemoUITests.xctest".into()),
                arguments: Vec::new(),
                environment: BTreeMap::from([(
                    "FEATURE".into(),
                    XCTestEnvironmentValue::Boolean(true),
                )]),
                tests_to_run: vec!["DemoUITests/testExample".into()],
                tests_to_skip: Vec::new(),
                is_ui_test: true,
            }],
            output_path: Some("C:/captures/xctest.jsonl".into()),
            junit_output_path: Some("C:/captures/xctest-junit.xml".into()),
            source_xctestrun_path: None,
            timeout_ms: Some(60_000),
        }));
        assert_eq!(xctest.kind(), TaskKind::XCTest);
        let encoded = serde_json::to_value(&xctest).unwrap();
        assert_eq!(encoded["kind"], "xctest");
        assert_eq!(serde_json::from_value::<TaskSpec>(encoded).unwrap(), xctest);

        let app_launch = TaskSpec::AppLaunch(AppLaunchSpec {
            udid: Some("test-phone".into()),
            bundle_id: "com.example.Demo".into(),
            arguments: vec!["--example".into()],
            environment: BTreeMap::from([("FEATURE".into(), "true".into())]),
            kill_existing: true,
            output_path: Some("C:/captures/app.log".into()),
        });
        assert_eq!(app_launch.kind(), TaskKind::AppProcess);
        let encoded = serde_json::to_value(&app_launch).unwrap();
        assert_eq!(encoded["kind"], "app_launch");
        assert_eq!(
            serde_json::from_value::<TaskSpec>(encoded).unwrap(),
            app_launch
        );
    }
}
