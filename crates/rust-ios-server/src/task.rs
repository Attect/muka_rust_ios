use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rust_ios_core::{
    AppProcessSessionInfo, CoreError, CoreResult, DeviceBackend, DeviceTarget, DproxyCaptureSpec,
    LocationSimulationInfo, RsdServiceInfo, RsdServiceKind, UiRunnerSession, UiRunnerSessionInfo,
    UserSpaceTunnel, WdaSession, WdaSessionInfo, XCTestSessionInfo, XCTestSessionUpdate,
    bridge_streams, run_dproxy_connection,
};
use rust_ios_protocol::{
    AccessibilityInspectorSpec, AppLaunchSpec, CommandInvocation, CommandResult, DebugProxySpec,
    DesiredState, DeviceStateSpec, DproxyMode, DproxySpec, ErrorCode, ForwardSpec,
    LocationSimulationPoint, LocationSimulationSpec, ProtocolError, RemotePairingInfo,
    RestartPolicy, RuntimeState, ScreenshotStreamSpec, StreamFormat, StreamTaskSpec, TaskKind,
    TaskOutput, TaskRecord, TaskSelector, TaskSpec, TunnelMode, TunnelPairSpec, TunnelSpec,
    TunnelTransport, UiAutomationDriver, UiRunnerSpec, UiRunnerTarget, UiStreamFormat,
    UiStreamSpec, WdaSpec, WebInspectorCdpSpec, WebInspectorJsShellSpec, XCTestRunSummary,
    XCTestSpec,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{PersistedState, StateStore, unix_time_ms};

#[derive(Debug, Error)]
pub enum TaskManagerError {
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task conflicts with an existing task: {0}")]
    Conflict(String),
    #[error("task persistence failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl From<TaskManagerError> for ProtocolError {
    fn from(error: TaskManagerError) -> Self {
        match error {
            TaskManagerError::NotFound(message) => ProtocolError::new(ErrorCode::NotFound, message),
            TaskManagerError::Conflict(message) => ProtocolError::new(ErrorCode::Conflict, message),
            TaskManagerError::Io(error) => {
                ProtocolError::new(ErrorCode::Internal, error.to_string())
            }
            TaskManagerError::Core(error) => error.into(),
        }
    }
}

#[derive(Debug)]
struct RuntimeHandle {
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    js_shell_input: Option<mpsc::Sender<WebInspectorJsShellCommand>>,
}

#[derive(Debug)]
struct WebInspectorJsShellCommand {
    expression: String,
    response: oneshot::Sender<CoreResult<Value>>,
}

#[derive(Debug)]
struct ManagedTask {
    record: Arc<RwLock<TaskRecord>>,
    runtime: Mutex<RuntimeHandle>,
}

#[derive(Debug, Clone)]
struct ActiveTunnel {
    task_id: Uuid,
    tunnel: Arc<dyn UserSpaceTunnel>,
}

#[derive(Debug, Default)]
struct DproxyRuntimeCounters {
    accepted: AtomicU64,
    active: AtomicU64,
    completed: AtomicU64,
    failed: AtomicU64,
    host_to_usbmuxd_bytes: AtomicU64,
    usbmuxd_to_host_bytes: AtomicU64,
}

#[derive(Debug)]
pub struct TaskManager {
    backend: Arc<dyn DeviceBackend>,
    store: StateStore,
    output_root: PathBuf,
    pair_record_root: PathBuf,
    tasks: RwLock<HashMap<Uuid, Arc<ManagedTask>>>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
    accessibility_start_lock: Mutex<()>,
}

impl TaskManager {
    pub async fn load(
        backend: Arc<dyn DeviceBackend>,
        store: StateStore,
    ) -> Result<Arc<Self>, TaskManagerError> {
        let persisted = store.load_or_quarantine()?;
        let server_root = store.path().parent().unwrap_or_else(|| Path::new("."));
        let output_root = server_root.join("captures");
        let pair_record_root = server_root.join("remote-pairing");
        let manager = Arc::new(Self {
            backend,
            store,
            output_root,
            pair_record_root,
            tasks: RwLock::new(HashMap::new()),
            active_tunnels: Arc::new(RwLock::new(HashMap::new())),
            accessibility_start_lock: Mutex::new(()),
        });
        manager.restore(persisted).await;
        Ok(manager)
    }

    async fn restore(self: &Arc<Self>, persisted: PersistedState) {
        let mut records = persisted.tasks;
        let mut finite_state_changed = false;
        records.sort_by_key(|record| !matches!(record.spec, TaskSpec::Tunnel(_)));
        for mut record in records {
            if record.restart_policy == RestartPolicy::Never {
                if record.desired_state == DesiredState::Running {
                    record.desired_state = DesiredState::Stopped;
                    record.runtime_state = RuntimeState::Failed;
                    record.updated_at_ms = unix_time_ms();
                    record.message = Some("finite task was interrupted by server restart".into());
                    finite_state_changed = true;
                }
                let managed = Arc::new(ManagedTask {
                    record: Arc::new(RwLock::new(record)),
                    runtime: Mutex::new(RuntimeHandle {
                        cancel: CancellationToken::new(),
                        join: None,
                        js_shell_input: None,
                    }),
                });
                let id = managed.record.read().await.id;
                self.tasks.write().await.insert(id, managed);
                continue;
            }
            if record.desired_state != DesiredState::Running {
                continue;
            }
            let is_tunnel = matches!(record.spec, TaskSpec::Tunnel(_));
            record.runtime_state = RuntimeState::Starting;
            record.generation = record.generation.saturating_add(1);
            record.attempts = 0;
            record.updated_at_ms = unix_time_ms();
            record.started_at_ms = None;
            record.message = Some("restored after server start".into());
            record.runtime = Value::Null;
            self.ensure_stream_output_path(&mut record);
            let managed = Arc::new(ManagedTask {
                record: Arc::new(RwLock::new(record)),
                runtime: Mutex::new(RuntimeHandle {
                    cancel: CancellationToken::new(),
                    join: None,
                    js_shell_input: None,
                }),
            });
            let id = managed.record.read().await.id;
            self.tasks.write().await.insert(id, managed.clone());
            self.spawn(managed.clone(), None).await;
            if is_tunnel {
                wait_until_started(&managed.record, Duration::from_secs(5)).await;
            }
        }
        if finite_state_changed && let Err(error) = self.persist().await {
            warn!(%error, "failed to persist interrupted finite task state");
        }
    }

    pub async fn start_forward_many(
        self: &Arc<Self>,
        specs: Vec<ForwardSpec>,
    ) -> Result<Vec<TaskRecord>, TaskManagerError> {
        if specs.is_empty() {
            return Err(CoreError::InvalidArgument("no forward mappings supplied".into()).into());
        }
        let mut started = Vec::new();
        for spec in specs {
            match self.start_forward(spec).await {
                Ok(record) => started.push(record),
                Err(error) => {
                    for record in &started {
                        let _ = self.stop(TaskSelector::Id(record.id)).await;
                    }
                    return Err(error);
                }
            }
        }
        Ok(started)
    }

    pub async fn start_forward(
        self: &Arc<Self>,
        mut spec: ForwardSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.device_port == 0 {
            return Err(CoreError::InvalidArgument("device port cannot be 0".into()).into());
        }
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.host_port))
            .await
            .map_err(|error| {
                TaskManagerError::Conflict(format!(
                    "cannot bind {}:{}: {error}",
                    spec.bind_host, spec.host_port
                ))
            })?;
        spec.host_port = listener.local_addr()?.port();

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if let TaskSpec::Forward(existing) = &record.spec
                && existing.bind_host == spec.bind_host
                && existing.host_port == spec.host_port
            {
                return Err(TaskManagerError::Conflict(format!(
                    "{}:{} is already forwarded by task {}",
                    spec.bind_host, spec.host_port, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self
            .insert_record(new_record(TaskSpec::Forward(spec)))
            .await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        let result = managed.record.read().await.clone();
        Ok(result)
    }

    pub async fn start_tunnel(
        self: &Arc<Self>,
        mut spec: TunnelSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.transport == TunnelTransport::RemotePairingTcp {
            if spec.mode == TunnelMode::Kernel {
                return Err(CoreError::InvalidArgument(
                    "RemotePairingTcp only supports userspace tunnel mode".into(),
                )
                .into());
            }
            self.normalize_pair_record_path(&mut spec.pair_record_path)?;
        }
        self.normalize_kernel_tunnel_options(&mut spec)?;
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if let TaskSpec::Tunnel(existing) = &record.spec
                && existing.udid == spec.udid
            {
                return Err(TaskManagerError::Conflict(format!(
                    "tunnel selector {:?} is already managed by task {}",
                    spec.udid, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self.insert_record(new_record(TaskSpec::Tunnel(spec))).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(150)).await;
        let result = managed.record.read().await.clone();
        Ok(result)
    }

    pub async fn pair_remote_tunnel(
        &self,
        mut spec: TunnelPairSpec,
    ) -> Result<RemotePairingInfo, TaskManagerError> {
        if !spec.manual {
            return Err(CoreError::InvalidArgument(
                "tunnel pair requires explicit --manual authorization".into(),
            )
            .into());
        }
        self.normalize_pair_record_path(&mut spec.pair_record_path)?;
        self.backend
            .pair_remote_tunnel(&spec)
            .await
            .map_err(TaskManagerError::from)
    }

    fn normalize_pair_record_path(&self, value: &mut String) -> CoreResult<()> {
        let trimmed = value.trim();
        let path = if trimmed.is_empty() || matches!(trimmed, "." | "default") {
            self.pair_record_root.clone()
        } else {
            let path = PathBuf::from(trimmed);
            if !path.is_absolute() {
                return Err(CoreError::InvalidArgument(
                    "pair record path must be absolute; CLI clients resolve relative paths".into(),
                ));
            }
            path
        };
        *value = path.to_string_lossy().into_owned();
        Ok(())
    }

    fn normalize_kernel_tunnel_options(&self, spec: &mut TunnelSpec) -> CoreResult<()> {
        if spec.mode != TunnelMode::Kernel {
            if spec.kernel_interface_name.is_some() || spec.wintun_path.is_some() {
                return Err(CoreError::InvalidArgument(
                    "--interface-name and --wintun-path require kernel tunnel mode".into(),
                ));
            }
            return Ok(());
        }
        if let Some(name) = spec.kernel_interface_name.as_mut() {
            *name = name.trim().to_owned();
            if name.is_empty() {
                return Err(CoreError::InvalidArgument(
                    "kernel interface name cannot be empty".into(),
                ));
            }
            if name.chars().any(char::is_control) {
                return Err(CoreError::InvalidArgument(
                    "kernel interface name cannot contain control characters".into(),
                ));
            }
        }
        if let Some(value) = spec.wintun_path.as_mut() {
            *value = value.trim().to_owned();
            if value.is_empty() {
                spec.wintun_path = None;
            } else if !Path::new(value.as_str()).is_absolute() {
                return Err(CoreError::InvalidArgument(
                    "Wintun DLL path must be absolute; CLI clients resolve relative paths".into(),
                ));
            }
        }
        Ok(())
    }

    pub async fn start_device_state(
        self: &Arc<Self>,
        spec: DeviceStateSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.condition_identifier.is_empty() || spec.profile_identifier.is_empty() {
            return Err(CoreError::InvalidArgument(
                "devicestate condition and profile identifiers cannot be empty".into(),
            )
            .into());
        }
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if let TaskSpec::DeviceState(existing) = &record.spec
                && existing.udid == spec.udid
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has devicestate task {}",
                    spec.udid, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self
            .insert_record(new_record(TaskSpec::DeviceState(spec)))
            .await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(150)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_location_simulation(
        self: &Arc<Self>,
        spec: LocationSimulationSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        validate_location_simulation_spec(&spec)?;
        let mut record =
            new_record_with_policy(TaskSpec::LocationSimulation(spec), RestartPolicy::Never);
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::LocationSimulation(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized location simulation output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "location simulation output path must be absolute".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
            if let TaskSpec::LocationSimulation(existing_spec) = &existing.spec
                && existing.desired_state == DesiredState::Running
                && device_selectors_overlap(&existing_spec.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has active location simulation task {}",
                    spec.udid, existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(500)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_screenshot_stream(
        self: &Arc<Self>,
        mut spec: ScreenshotStreamSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if !(16..=5_000).contains(&spec.interval_ms) {
            return Err(CoreError::InvalidArgument(
                "screenshot stream interval must be 16..5000 ms".into(),
            )
            .into());
        }
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.port))
            .await
            .map_err(|error| {
                TaskManagerError::Conflict(format!(
                    "cannot bind screenshot stream {}:{}: {error}",
                    spec.bind_host, spec.port
                ))
            })?;
        spec.port = listener.local_addr()?.port();
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if let TaskSpec::ScreenshotStream(existing) = &record.spec
                && existing.bind_host == spec.bind_host
                && existing.port == spec.port
            {
                return Err(TaskManagerError::Conflict(format!(
                    "screenshot stream {}:{} is already owned by task {}",
                    spec.bind_host, spec.port, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self
            .insert_record(new_record(TaskSpec::ScreenshotStream(spec)))
            .await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_webinspector_cdp(
        self: &Arc<Self>,
        mut spec: WebInspectorCdpSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.bind_host.is_empty() {
            return Err(CoreError::InvalidArgument(
                "WebInspector CDP bind host cannot be empty".into(),
            )
            .into());
        }
        if !(1_000..=120_000).contains(&spec.startup_timeout_ms) {
            return Err(CoreError::InvalidArgument(
                "WebInspector CDP startup timeout must be 1000..120000 ms".into(),
            )
            .into());
        }
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.port))
            .await
            .map_err(|error| {
                CoreError::Io(io::Error::new(
                    error.kind(),
                    format!(
                        "bind WebInspector CDP endpoint {}:{}: {error}",
                        spec.bind_host, spec.port
                    ),
                ))
            })?;
        spec.port = listener.local_addr()?.port();

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if record.desired_state != DesiredState::Running {
                continue;
            }
            if let TaskSpec::WebInspectorCdp(existing) = &record.spec
                && device_selectors_overlap(&existing.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has WebInspector CDP task {}",
                    spec.udid, record.id
                )));
            }
            // 同设备的 CDP 与 JS shell 互斥：webinspector service 每设备只允许
            // 一个连接，占用时第二个连接会被设备挂起直到超时（2026-08-10 真机
            // 观察到 js-shell 在 CDP 运行时 20 秒超时而非明确失败）。
            if let TaskSpec::WebInspectorJsShell(existing) = &record.spec
                && device_selectors_overlap(&existing.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has WebInspector JS shell task {}; the webinspector service allows a single connection per device",
                    spec.udid, record.id
                )));
            }
            if let Some((host, port)) = listening_endpoint(&record.spec)
                && host == spec.bind_host
                && port == spec.port
            {
                return Err(TaskManagerError::Conflict(format!(
                    "WebInspector CDP endpoint {}:{} is already owned by task {}",
                    spec.bind_host, spec.port, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self
            .insert_record(new_record(TaskSpec::WebInspectorCdp(spec)))
            .await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_webinspector_js_shell(
        self: &Arc<Self>,
        spec: WebInspectorJsShellSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        validate_webinspector_js_shell_spec(&spec)?;
        let mut record =
            new_record_with_policy(TaskSpec::WebInspectorJsShell(spec), RestartPolicy::Never);
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::WebInspectorJsShell(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized WebInspector JS shell output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "WebInspector JS shell output path must be absolute".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if existing.desired_state != DesiredState::Running {
                continue;
            }
            if let TaskSpec::WebInspectorJsShell(existing_spec) = &existing.spec
                && device_selectors_overlap(&existing_spec.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has WebInspector JS shell task {}",
                    spec.udid, existing.id
                )));
            }
            // 同设备的 CDP 与 JS shell 互斥：webinspector service 每设备只允许
            // 一个连接（见 start_web_inspector_cdp 的注释）。
            if let TaskSpec::WebInspectorCdp(existing_spec) = &existing.spec
                && device_selectors_overlap(&existing_spec.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has WebInspector CDP task {}; the webinspector service allows a single connection per device",
                    spec.udid, existing.id
                )));
            }
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn webinspector_js_shell_eval(
        &self,
        task_id: Uuid,
        expression: String,
    ) -> Result<CommandResult, TaskManagerError> {
        if expression.trim().is_empty() {
            return Err(CoreError::InvalidArgument(
                "WebInspector JS shell expression cannot be empty".into(),
            )
            .into());
        }
        if expression.len() > 1024 * 1024 {
            return Err(CoreError::InvalidArgument(
                "WebInspector JS shell expression exceeds 1 MiB".into(),
            )
            .into());
        }
        let task = self
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or_else(|| TaskManagerError::NotFound(task_id.to_string()))?;
        let timeout_ms = {
            let record = task.record.read().await;
            let TaskSpec::WebInspectorJsShell(spec) = &record.spec else {
                return Err(CoreError::InvalidArgument(format!(
                    "task {task_id} is not a WebInspector JS shell"
                ))
                .into());
            };
            if record.desired_state != DesiredState::Running
                || record.runtime_state != RuntimeState::Running
            {
                return Err(CoreError::DeviceUnavailable(format!(
                    "WebInspector JS shell task {task_id} is {:?}: {}",
                    record.runtime_state,
                    record.message.as_deref().unwrap_or("not ready")
                ))
                .into());
            }
            spec.command_timeout_ms
        };
        let input = task
            .runtime
            .lock()
            .await
            .js_shell_input
            .clone()
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!(
                    "WebInspector JS shell task {task_id} has no active input channel"
                ))
            })?;
        let (response, receiver) = oneshot::channel();
        input
            .send(WebInspectorJsShellCommand {
                expression,
                response,
            })
            .await
            .map_err(|_| {
                CoreError::DeviceUnavailable(format!(
                    "WebInspector JS shell task {task_id} stopped before accepting input"
                ))
            })?;
        let value = timeout(
            Duration::from_millis(timeout_ms.saturating_add(1_000)),
            receiver,
        )
        .await
        .map_err(|_| {
            CoreError::Timeout(format!(
                "WebInspector JS shell task {task_id} evaluation response"
            ))
        })?
        .map_err(|_| {
            CoreError::DeviceUnavailable(format!(
                "WebInspector JS shell task {task_id} stopped before replying"
            ))
        })??;
        Ok(CommandResult {
            exit_code: 0,
            value: Some(value),
            text: None,
            warnings: Vec::new(),
        })
    }

    pub async fn start_debug_proxy(
        self: &Arc<Self>,
        mut spec: DebugProxySpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.bind_host.is_empty() {
            return Err(
                CoreError::InvalidArgument("debug proxy bind host cannot be empty".into()).into(),
            );
        }
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.port))
            .await
            .map_err(|error| {
                TaskManagerError::Conflict(format!(
                    "cannot bind debug proxy {}:{}: {error}",
                    spec.bind_host, spec.port
                ))
            })?;
        spec.port = listener.local_addr()?.port();
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if let TaskSpec::DebugProxy(existing) = &record.spec
                && existing.udid == spec.udid
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has debug proxy task {}",
                    spec.udid, record.id
                )));
            }
        }
        drop(tasks);

        let managed = self
            .insert_record(new_record(TaskSpec::DebugProxy(spec)))
            .await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_dproxy(
        self: &Arc<Self>,
        mut spec: DproxySpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        spec.bind_host = spec.bind_host.trim().to_owned();
        if spec.bind_host.is_empty() {
            return Err(
                CoreError::InvalidArgument("dproxy bind host cannot be empty".into()).into(),
            );
        }
        spec.upstream_address = spec.upstream_address.trim().to_owned();
        let upstream = spec
            .upstream_address
            .parse::<std::net::SocketAddr>()
            .map_err(|error| {
                CoreError::InvalidArgument(format!(
                    "dproxy upstream must be an IP socket address: {error}"
                ))
            })?;
        if spec.mode == DproxyMode::Utun {
            return Err(CoreError::NotImplemented(
                "dproxy --mode=utun is not implemented on Windows; use --mode=usbmuxd".into(),
            )
            .into());
        }
        if spec.interface.is_some() {
            return Err(CoreError::InvalidArgument(
                "dproxy --iface only applies to the unimplemented utun mode".into(),
            )
            .into());
        }
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.port))
            .await
            .map_err(|error| {
                TaskManagerError::Conflict(format!(
                    "cannot bind dproxy {}:{}: {error}",
                    spec.bind_host, spec.port
                ))
            })?;
        let local_address = listener.local_addr()?;
        if !local_address.ip().is_loopback() {
            return Err(CoreError::InvalidArgument(
                "dproxy listener must use a loopback address".into(),
            )
            .into());
        }
        if socket_addresses_overlap(local_address, upstream) {
            return Err(CoreError::InvalidArgument(
                "dproxy listener and upstream cannot be the same address".into(),
            )
            .into());
        }
        spec.port = local_address.port();

        let mut record = new_record(TaskSpec::Dproxy(spec));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::Dproxy(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized dproxy output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "dproxy output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
        }
        drop(tasks);
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_wda(
        self: &Arc<Self>,
        spec: WdaSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        if spec.runner_bundle_id.trim().is_empty() {
            return Err(
                CoreError::InvalidArgument("WDA runner bundle id cannot be empty".into()).into(),
            );
        }
        if !(1_000..=300_000).contains(&spec.readiness_timeout_ms) {
            return Err(CoreError::InvalidArgument(
                "WDA readiness timeout must be 1000..300000 ms".into(),
            )
            .into());
        }
        if let Some((key, _)) = spec
            .environment
            .iter()
            .find(|(key, _)| key.is_empty() || key.contains('='))
        {
            return Err(
                CoreError::InvalidArgument(format!("invalid WDA environment key {key:?}")).into(),
            );
        }
        let mut record = new_record(TaskSpec::Wda(Box::new(spec)));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::Wda(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized WDA output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "WDA output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }
        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let record = task.record.read().await;
            if task_output_path(&record.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    record.id
                )));
            }
            if let TaskSpec::Wda(existing) = &record.spec
                && device_selectors_overlap(&existing.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has WDA task {}",
                    spec.udid, record.id
                )));
            }
            if let TaskSpec::XCTest(existing) = &record.spec
                && record.desired_state == DesiredState::Running
                && device_selectors_overlap(&existing.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has XCTest task {}",
                    spec.udid, record.id
                )));
            }
            if let TaskSpec::UiRunner(existing) = &record.spec
                && record.desired_state == DesiredState::Running
                && device_selectors_overlap(&existing.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has UI runner task {}",
                    spec.udid, record.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_ui_runner(
        self: &Arc<Self>,
        mut spec: UiRunnerSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        validate_ui_runner_spec(&spec)?;
        let listener = TcpListener::bind((spec.bind_host.as_str(), spec.host_port))
            .await
            .map_err(|error| {
                TaskManagerError::Conflict(format!(
                    "cannot bind UI runner endpoint {}:{}: {error}",
                    spec.bind_host, spec.host_port
                ))
            })?;
        spec.host_port = listener.local_addr()?.port();

        let mut record = new_record(TaskSpec::UiRunner(Box::new(spec)));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::UiRunner(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized UI runner output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "UI runner output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if let Some((host, port)) = listening_endpoint(&existing.spec)
                && host == spec.bind_host
                && port == spec.host_port
            {
                return Err(TaskManagerError::Conflict(format!(
                    "UI runner endpoint {}:{} is already owned by task {}",
                    spec.bind_host, spec.host_port, existing.id
                )));
            }
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
            let same_device = match &existing.spec {
                TaskSpec::Wda(existing) => device_selectors_overlap(&existing.udid, &spec.udid),
                TaskSpec::UiRunner(existing) => {
                    device_selectors_overlap(&existing.udid, &spec.udid)
                }
                TaskSpec::XCTest(existing_spec) => {
                    device_selectors_overlap(&existing_spec.udid, &spec.udid)
                }
                _ => false,
            };
            if same_device && existing.desired_state == DesiredState::Running {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has active WDA/XCTest/UI runner task {}",
                    spec.udid, existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), Some(listener)).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_xctest(
        self: &Arc<Self>,
        spec: XCTestSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        validate_xctest_spec(&spec)?;
        let mut record =
            new_record_with_policy(TaskSpec::XCTest(Box::new(spec)), RestartPolicy::Never);
        self.ensure_xctest_output_path(&mut record);
        let TaskSpec::XCTest(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized XCTest output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "XCTest output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing_record = task.record.read().await;
            let output_conflict =
                task_output_path(&existing_record.spec) == spec.output_path.as_deref();
            if output_conflict {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing_record.id
                )));
            }
            let same_device = match &existing_record.spec {
                TaskSpec::Wda(existing) => device_selectors_overlap(&existing.udid, &spec.udid),
                TaskSpec::UiRunner(existing) => {
                    device_selectors_overlap(&existing.udid, &spec.udid)
                }
                TaskSpec::XCTest(existing) => {
                    device_selectors_overlap(&existing.udid, &spec.udid)
                        && existing_record.desired_state == DesiredState::Running
                }
                _ => false,
            };
            if same_device {
                return Err(TaskManagerError::Conflict(format!(
                    "device selector {:?} already has active WDA/XCTest/UI runner task {}",
                    spec.udid, existing_record.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_app_launch(
        self: &Arc<Self>,
        mut spec: AppLaunchSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        validate_app_launch_spec(&spec)?;
        let (udid, _) = lookup_active_tunnel(&self.active_tunnels, spec.udid.as_deref()).await?;
        spec.udid = Some(udid);

        let mut record = new_record_with_policy(TaskSpec::AppLaunch(spec), RestartPolicy::Never);
        self.ensure_app_launch_output_path(&mut record);
        let TaskSpec::AppLaunch(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized app process output path"),
        );
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "app process output path must be absolute; CLI clients resolve relative paths"
                    .into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            let output_conflict = task_output_path(&existing.spec) == spec.output_path.as_deref();
            if output_conflict {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
            if let TaskSpec::AppLaunch(existing_spec) = &existing.spec
                && existing.desired_state == DesiredState::Running
                && existing_spec.udid == spec.udid
                && existing_spec.bundle_id == spec.bundle_id
            {
                return Err(TaskManagerError::Conflict(format!(
                    "app {} already has active process task {} on {:?}",
                    spec.bundle_id, existing.id, spec.udid
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_secs(1)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_stream(
        self: &Arc<Self>,
        spec: StreamTaskSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        match (&spec.kind, &spec.format) {
            (
                TaskKind::Syslog
                | TaskKind::OsTrace
                | TaskKind::Notifications
                | TaskKind::Sysmontap,
                StreamFormat::Raw | StreamFormat::JsonLines,
            )
            | (TaskKind::Pcap, StreamFormat::Pcap) => {}
            (
                TaskKind::Syslog
                | TaskKind::OsTrace
                | TaskKind::Notifications
                | TaskKind::Sysmontap,
                StreamFormat::Pcap,
            ) => {
                return Err(CoreError::InvalidArgument(
                    "log streams do not support pcap format".into(),
                )
                .into());
            }
            (TaskKind::Pcap, _) => {
                return Err(
                    CoreError::InvalidArgument("pcap stream requires pcap format".into()).into(),
                );
            }
            (kind, _) => {
                return Err(CoreError::NotImplemented(format!(
                    "stream task kind {kind:?} is not implemented"
                ))
                .into());
            }
        }
        if spec.process.as_ref().is_some_and(|value| value.is_empty()) {
            return Err(CoreError::InvalidArgument("process filter cannot be empty".into()).into());
        }
        if spec.kind != TaskKind::OsTrace && spec.os_trace.is_some() {
            return Err(CoreError::InvalidArgument(
                "os_trace options are only valid for an ostrace stream".into(),
            )
            .into());
        }
        if let Some(options) = &spec.os_trace {
            for (name, value) in [
                ("subsystem", options.subsystem_contains.as_deref()),
                ("match", options.message_contains.as_deref()),
                ("exclude", options.message_excludes.as_deref()),
            ] {
                if value.is_some_and(|value| value.is_empty() || value.len() > 64 * 1024) {
                    return Err(CoreError::InvalidArgument(format!(
                        "ostrace --{name} filter must be non-empty and at most 64 KiB"
                    ))
                    .into());
                }
            }
        }

        let mut record = new_record(TaskSpec::Stream(spec));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::Stream(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(spec.output_path.as_deref().expect("normalized output path"));
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "stream output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            let output_conflict = task_output_path(&existing.spec) == spec.output_path.as_deref();
            if output_conflict {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(150)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_accessibility_inspector(
        self: &Arc<Self>,
        spec: AccessibilityInspectorSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        let _start_guard = self.accessibility_start_lock.lock().await;
        if let Some(font_size) = spec.font_size
            && (!font_size.is_finite() || font_size <= 0.0)
        {
            return Err(CoreError::InvalidArgument(
                "ax --font must be a finite number greater than zero".into(),
            )
            .into());
        }
        let mut record = new_record(TaskSpec::AccessibilityInspector(spec));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::AccessibilityInspector(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(spec.output_path.as_deref().expect("normalized output path"));
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "Accessibility inspector output path must be absolute".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if let TaskSpec::AccessibilityInspector(existing_spec) = &existing.spec
                && device_selectors_overlap(&existing_spec.udid, &spec.udid)
            {
                return Err(TaskManagerError::Conflict(format!(
                    "Accessibility inspector task {} already owns the selected device",
                    existing.id
                )));
            }
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;
        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(150)).await;
        Ok(managed.record.read().await.clone())
    }

    pub async fn start_ui_stream(
        self: &Arc<Self>,
        mut spec: UiStreamSpec,
    ) -> Result<TaskRecord, TaskManagerError> {
        spec.wda_url = normalize_ui_base_url(&spec.wda_url, "WDA")?;
        spec.devicekit_url = normalize_ui_base_url(&spec.devicekit_url, "DeviceKit")?;
        if spec.wda_task_id.is_none() && spec.driver != UiAutomationDriver::DeviceKit {
            let tasks = self.tasks.read().await;
            for task in tasks.values() {
                let existing = task.record.read().await;
                if existing.kind == TaskKind::Wda
                    && existing.desired_state == DesiredState::Running
                    && existing.runtime["wdaUrl"].as_str() == Some(spec.wda_url.as_str())
                {
                    spec.wda_task_id = Some(existing.id);
                    break;
                }
            }
        }
        if spec.driver == UiAutomationDriver::Wda && spec.format == UiStreamFormat::H264 {
            return Err(CoreError::InvalidArgument(
                "WDA UI stream supports MJPEG only; use DeviceKit for H264".into(),
            )
            .into());
        }
        if spec.fps.is_some_and(|value| !(1..=240).contains(&value)) {
            return Err(CoreError::InvalidArgument("UI stream fps must be 1..240".into()).into());
        }
        if spec
            .quality
            .is_some_and(|value| !(1..=100).contains(&value))
        {
            return Err(
                CoreError::InvalidArgument("UI stream quality must be 1..100".into()).into(),
            );
        }
        if spec
            .scale
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(CoreError::InvalidArgument(
                "UI stream scale must be a finite positive number".into(),
            )
            .into());
        }
        if spec.bitrate == Some(0) {
            return Err(
                CoreError::InvalidArgument("UI stream bitrate must be positive".into()).into(),
            );
        }

        let mut record = new_record(TaskSpec::UiStream(spec));
        self.ensure_stream_output_path(&mut record);
        let TaskSpec::UiStream(spec) = &record.spec else {
            unreachable!();
        };
        let output = PathBuf::from(spec.output_path.as_deref().expect("normalized output path"));
        if !output.is_absolute() {
            return Err(CoreError::InvalidArgument(
                "UI stream output path must be absolute; CLI clients resolve relative paths".into(),
            )
            .into());
        }

        let tasks = self.tasks.read().await;
        for task in tasks.values() {
            let existing = task.record.read().await;
            if task_output_path(&existing.spec) == spec.output_path.as_deref() {
                return Err(TaskManagerError::Conflict(format!(
                    "output {} is already owned by task {}",
                    output.display(),
                    existing.id
                )));
            }
        }
        drop(tasks);

        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&output)
            .await?;

        let managed = self.insert_record(record).await;
        self.persist().await?;
        self.spawn(managed.clone(), None).await;
        wait_until_started(&managed.record, Duration::from_millis(500)).await;
        Ok(managed.record.read().await.clone())
    }

    fn ensure_stream_output_path(&self, record: &mut TaskRecord) {
        match &mut record.spec {
            TaskSpec::Stream(spec) if spec.output_path.is_none() => {
                let extension = match spec.kind {
                    TaskKind::Pcap => "pcap",
                    _ => "log",
                };
                let name = format!("{}-{}.{}", spec.kind.as_str(), record.id, extension);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::AccessibilityInspector(spec) if spec.output_path.is_none() => {
                let name = format!("accessibility-inspector-{}.jsonl", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::LocationSimulation(spec) if spec.output_path.is_none() => {
                let name = format!("location-simulation-{}.jsonl", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::UiStream(spec) if spec.output_path.is_none() => {
                let name = format!("ui-stream-{}.{}", record.id, spec.format.as_str());
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::UiRunner(spec) if spec.output_path.is_none() => {
                let name = format!("ui-runner-{}.jsonl", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::Wda(spec) if spec.output_path.is_none() => {
                let name = format!("wda-{}.jsonl", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::WebInspectorJsShell(spec) if spec.output_path.is_none() => {
                let name = format!("webinspector-js-shell-{}.jsonl", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            TaskSpec::Dproxy(spec) if spec.output_path.is_none() => {
                let name = format!("dproxy-{}.ndjson", record.id);
                spec.output_path = Some(self.output_root.join(name).to_string_lossy().into_owned());
            }
            _ => {}
        }
    }

    fn ensure_xctest_output_path(&self, record: &mut TaskRecord) {
        let TaskSpec::XCTest(spec) = &mut record.spec else {
            return;
        };
        if spec.output_path.is_none() {
            spec.output_path = Some(
                self.output_root
                    .join(format!("xctest-{}.jsonl", record.id))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    fn ensure_app_launch_output_path(&self, record: &mut TaskRecord) {
        let TaskSpec::AppLaunch(spec) = &mut record.spec else {
            return;
        };
        if spec.output_path.is_none() {
            spec.output_path = Some(
                self.output_root
                    .join(format!("app-process-{}.log", record.id))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }

    async fn insert_record(&self, record: TaskRecord) -> Arc<ManagedTask> {
        let id = record.id;
        let managed = Arc::new(ManagedTask {
            record: Arc::new(RwLock::new(record)),
            runtime: Mutex::new(RuntimeHandle {
                cancel: CancellationToken::new(),
                join: None,
                js_shell_input: None,
            }),
        });
        self.tasks.write().await.insert(id, managed.clone());
        managed
    }

    async fn spawn(self: &Arc<Self>, task: Arc<ManagedTask>, prebound: Option<TcpListener>) {
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let record = task.record.clone();
        let backend = self.backend.clone();
        let active_tunnels = self.active_tunnels.clone();
        let manager = Arc::downgrade(self);
        let is_js_shell = matches!(record.read().await.spec, TaskSpec::WebInspectorJsShell(_));
        let (js_shell_input, js_shell_commands) = if is_js_shell {
            let (sender, receiver) = mpsc::channel(16);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let join = tokio::spawn(async move {
            let spec = record.read().await.spec.clone();
            let persist_terminal = matches!(
                &spec,
                TaskSpec::XCTest(_)
                    | TaskSpec::AppLaunch(_)
                    | TaskSpec::LocationSimulation(_)
                    | TaskSpec::WebInspectorJsShell(_)
            );
            match spec {
                TaskSpec::Forward(spec) => {
                    supervise_forward(record, backend, spec, child_cancel, prebound).await;
                }
                TaskSpec::Tunnel(spec) => {
                    supervise_tunnel(record, backend, spec, child_cancel, active_tunnels.clone())
                        .await;
                }
                TaskSpec::Stream(spec) => {
                    supervise_stream(
                        record,
                        backend,
                        ManagedStreamSpec::Stream(spec),
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::AccessibilityInspector(spec) => {
                    supervise_stream(
                        record,
                        backend,
                        ManagedStreamSpec::AccessibilityInspector(spec),
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::LocationSimulation(spec) => {
                    supervise_location_simulation(
                        record.clone(),
                        backend,
                        spec,
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::UiStream(spec) => {
                    supervise_ui_stream(record, spec, child_cancel, manager.clone()).await;
                }
                TaskSpec::UiRunner(spec) => {
                    supervise_ui_runner(
                        record,
                        backend,
                        *spec,
                        child_cancel,
                        prebound,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::ScreenshotStream(spec) => {
                    supervise_screenshot_stream(
                        record,
                        backend,
                        spec,
                        child_cancel,
                        prebound,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::WebInspectorCdp(spec) => {
                    supervise_webinspector_cdp(
                        record,
                        backend,
                        spec,
                        child_cancel,
                        prebound,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::WebInspectorJsShell(spec) => {
                    supervise_webinspector_js_shell(
                        record.clone(),
                        backend,
                        spec,
                        child_cancel,
                        js_shell_commands.expect("JS shell command receiver"),
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::DebugProxy(spec) => {
                    supervise_debug_proxy(
                        record,
                        spec,
                        child_cancel,
                        prebound,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::Dproxy(spec) => {
                    supervise_dproxy(record, spec, child_cancel, prebound).await;
                }
                TaskSpec::Wda(spec) => {
                    supervise_wda(record, backend, *spec, child_cancel, active_tunnels.clone())
                        .await;
                }
                TaskSpec::XCTest(spec) => {
                    supervise_xctest(
                        record.clone(),
                        backend,
                        *spec,
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::AppLaunch(spec) => {
                    supervise_app_launch(
                        record.clone(),
                        backend,
                        spec,
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::DeviceState(spec) => {
                    supervise_device_state(
                        record,
                        backend,
                        spec,
                        child_cancel,
                        active_tunnels.clone(),
                    )
                    .await;
                }
                TaskSpec::Generic(_) => {
                    set_state(
                        &record,
                        RuntimeState::Failed,
                        Some("generic task runner is not implemented".into()),
                        Value::Null,
                    )
                    .await;
                }
            }
            if persist_terminal
                && let Some(manager) = manager.upgrade()
                && let Err(error) = manager.persist().await
            {
                warn!(%error, "failed to persist terminal finite task");
            }
        });
        let mut runtime = task.runtime.lock().await;
        runtime.cancel = cancel;
        runtime.join = Some(join);
        runtime.js_shell_input = js_shell_input;
    }

    pub async fn list(&self, kind: Option<TaskKind>) -> Vec<TaskRecord> {
        let tasks: Vec<_> = self.tasks.read().await.values().cloned().collect();
        let mut records = Vec::with_capacity(tasks.len());
        for task in tasks {
            let record = task.record.read().await.clone();
            if kind.as_ref().is_none_or(|kind| record.kind == *kind) {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.created_at_ms);
        records
    }

    pub async fn execute(
        &self,
        invocation: &CommandInvocation,
    ) -> Result<CommandResult, TaskManagerError> {
        if let Some(result) = rust_ios_core::execute_host_command(invocation).await? {
            return Ok(result);
        }
        if invocation.argv.first().map(String::as_str) != Some("rsd") {
            let tunnel = if command_uses_shared_rsd(invocation) {
                lookup_optional_active_tunnel(&self.active_tunnels, invocation.udid.as_deref())
                    .await?
                    .map(|(_, tunnel)| tunnel)
            } else {
                None
            };
            return self
                .backend
                .execute_with_tunnel(invocation, tunnel)
                .await
                .map_err(Into::into);
        }
        if invocation.argv.get(1).map(String::as_str) != Some("ls") {
            return Err(CoreError::InvalidArgument("rsd requires the ls subcommand".into()).into());
        }
        if invocation
            .argv
            .iter()
            .skip(2)
            .any(|value| !value.starts_with('-'))
        {
            return Err(
                CoreError::InvalidArgument("rsd ls accepts no positional values".into()).into(),
            );
        }
        let (_, tunnel) =
            lookup_active_tunnel(&self.active_tunnels, invocation.udid.as_deref()).await?;
        let services: BTreeMap<String, RsdServiceInfo> = tunnel.rsd_services().await?;
        Ok(CommandResult {
            exit_code: 0,
            value: Some(
                serde_json::to_value(services).map_err(|error| {
                    CoreError::Backend(format!("serialize RSD services: {error}"))
                })?,
            ),
            text: None,
            warnings: Vec::new(),
        })
    }

    pub async fn show(&self, task_id: Uuid) -> Result<TaskRecord, TaskManagerError> {
        let task = self
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or_else(|| TaskManagerError::NotFound(task_id.to_string()))?;
        let result = task.record.read().await.clone();
        Ok(result)
    }

    pub async fn read_output(
        &self,
        task_id: Uuid,
        offset: u64,
        limit: u32,
    ) -> Result<TaskOutput, TaskManagerError> {
        const MAX_OUTPUT_READ: u32 = 1024 * 1024;
        if limit == 0 || limit > MAX_OUTPUT_READ {
            return Err(CoreError::InvalidArgument(format!(
                "output read limit must be between 1 and {MAX_OUTPUT_READ} bytes"
            ))
            .into());
        }
        let task = self
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or_else(|| TaskManagerError::NotFound(task_id.to_string()))?;
        let record = task.record.read().await;
        let (path, content_type) =
            match &record.spec {
                TaskSpec::Stream(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("stream output path is missing".into())
                    })?);
                    let content_type = record.runtime["contentType"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| stream_content_type(spec).into());
                    (path, content_type)
                }
                TaskSpec::AccessibilityInspector(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("Accessibility inspector output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::WebInspectorJsShell(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("WebInspector JS shell output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::Dproxy(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("dproxy output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::LocationSimulation(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("location simulation output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::UiStream(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("UI stream output path is missing".into())
                    })?);
                    let content_type = record.runtime["contentType"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| ui_stream_content_type(spec.format).into());
                    (path, content_type)
                }
                TaskSpec::UiRunner(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("UI runner output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::Wda(spec) => {
                    let path =
                        PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                            CoreError::Backend("WDA output path is missing".into())
                        })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::XCTest(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("XCTest output path is missing".into())
                    })?);
                    (path, "application/x-ndjson".into())
                }
                TaskSpec::AppLaunch(spec) => {
                    let path = PathBuf::from(spec.output_path.as_deref().ok_or_else(|| {
                        CoreError::Backend("app process output path is missing".into())
                    })?);
                    (path, "text/plain; charset=utf-8".into())
                }
                _ => {
                    return Err(CoreError::InvalidArgument(format!(
                        "task {task_id} does not produce captured output"
                    ))
                    .into());
                }
            };
        drop(record);

        let total_bytes = tokio::fs::metadata(&path).await?.len();
        if offset > total_bytes {
            return Err(CoreError::InvalidArgument(format!(
                "output offset {offset} exceeds file length {total_bytes}"
            ))
            .into());
        }
        let mut file = tokio::fs::File::open(&path).await?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let remaining = total_bytes.saturating_sub(offset).min(u64::from(limit)) as usize;
        let mut bytes = vec![0_u8; remaining];
        file.read_exact(&mut bytes).await?;
        let next_offset = offset + bytes.len() as u64;
        Ok(TaskOutput {
            task_id,
            offset,
            next_offset,
            total_bytes,
            eof: next_offset >= total_bytes,
            content_type,
            bytes_base64: BASE64.encode(bytes),
        })
    }

    pub async fn stop(&self, selector: TaskSelector) -> Result<Vec<TaskRecord>, TaskManagerError> {
        let candidates: Vec<_> = self.tasks.read().await.values().cloned().collect();
        let mut ids = Vec::new();
        for task in candidates {
            let record = task.record.read().await;
            if selector_matches(&selector, &record) {
                ids.push(record.id);
            }
        }
        if ids.is_empty() {
            return Err(TaskManagerError::NotFound(format!("{selector:?}")));
        }

        let mut stopped = Vec::new();
        for id in ids {
            let Some(task) = self.tasks.write().await.remove(&id) else {
                continue;
            };
            stop_runtime(&task).await;
            let mut record = task.record.write().await;
            record.desired_state = DesiredState::Stopped;
            record.runtime_state = RuntimeState::Stopped;
            record.updated_at_ms = unix_time_ms();
            record.message = Some("stopped by client".into());
            record.runtime = Value::Null;
            stopped.push(record.clone());
        }
        self.persist().await?;
        Ok(stopped)
    }

    pub async fn restart(self: &Arc<Self>, task_id: Uuid) -> Result<TaskRecord, TaskManagerError> {
        let task = self
            .tasks
            .read()
            .await
            .get(&task_id)
            .cloned()
            .ok_or_else(|| TaskManagerError::NotFound(task_id.to_string()))?;
        stop_runtime(&task).await;
        let finite_output = {
            let record = task.record.read().await;
            match &record.spec {
                TaskSpec::XCTest(spec) => spec.output_path.clone(),
                TaskSpec::AppLaunch(spec) => spec.output_path.clone(),
                TaskSpec::LocationSimulation(spec) => spec.output_path.clone(),
                TaskSpec::WebInspectorJsShell(spec) => spec.output_path.clone(),
                _ => None,
            }
        };
        if let Some(path) = finite_output {
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
                .await?;
        }
        {
            let mut record = task.record.write().await;
            record.desired_state = DesiredState::Running;
            record.runtime_state = RuntimeState::Starting;
            record.generation = record.generation.saturating_add(1);
            record.attempts = 0;
            record.updated_at_ms = unix_time_ms();
            record.started_at_ms = None;
            record.message = Some("restarting".into());
            record.runtime = Value::Null;
        }
        self.persist().await?;
        self.spawn(task.clone(), None).await;
        wait_until_started(&task.record, Duration::from_millis(150)).await;
        let result = task.record.read().await.clone();
        Ok(result)
    }

    pub async fn refresh_tunnels(
        self: &Arc<Self>,
        udid: Option<String>,
    ) -> Result<Vec<TaskRecord>, TaskManagerError> {
        let records = self.list(Some(TaskKind::Tunnel)).await;
        let ids: Vec<_> = records
            .into_iter()
            .filter(|record| match &record.spec {
                TaskSpec::Tunnel(spec) => udid
                    .as_ref()
                    .is_none_or(|value| spec.udid.as_ref().is_some_and(|item| item == value)),
                _ => false,
            })
            .map(|record| record.id)
            .collect();
        if ids.is_empty() {
            return Err(TaskManagerError::NotFound(
                udid.unwrap_or_else(|| "no tunnel task".into()),
            ));
        }
        let mut refreshed = Vec::new();
        for id in ids {
            refreshed.push(self.restart(id).await?);
        }
        Ok(refreshed)
    }

    pub async fn counts(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for record in self.list(None).await {
            *counts.entry(record.kind.as_str().to_owned()).or_insert(0) += 1;
        }
        counts
    }

    pub async fn shutdown(&self) {
        let tasks: Vec<_> = self.tasks.read().await.values().cloned().collect();
        for task in &tasks {
            task.runtime.lock().await.cancel.cancel();
        }
        for task in tasks {
            let join = task.runtime.lock().await.join.take();
            if let Some(join) = join {
                join_or_abort(join).await;
            }
        }
    }

    async fn persist(&self) -> Result<(), TaskManagerError> {
        let tasks = self
            .list(None)
            .await
            .into_iter()
            .filter(|record| {
                !matches!(
                    &record.spec,
                    TaskSpec::DebugProxy(spec) if spec.client_owned
                )
            })
            .collect();
        self.store.save(&PersistedState {
            schema_version: PersistedState::SCHEMA_VERSION,
            tasks,
        })?;
        Ok(())
    }
}

fn new_record(spec: TaskSpec) -> TaskRecord {
    new_record_with_policy(spec, RestartPolicy::Always)
}

fn new_record_with_policy(spec: TaskSpec, restart_policy: RestartPolicy) -> TaskRecord {
    let now = unix_time_ms();
    TaskRecord {
        id: Uuid::new_v4(),
        kind: spec.kind(),
        desired_state: DesiredState::Running,
        runtime_state: RuntimeState::Starting,
        restart_policy,
        generation: 1,
        attempts: 0,
        created_at_ms: now,
        updated_at_ms: now,
        started_at_ms: None,
        message: Some("task created".into()),
        spec,
        runtime: Value::Null,
    }
}

fn validate_xctest_spec(spec: &XCTestSpec) -> Result<(), TaskManagerError> {
    if spec.targets.is_empty() {
        return Err(
            CoreError::InvalidArgument("XCTest requires at least one target".into()).into(),
        );
    }
    if spec.targets.len() > 256 {
        return Err(CoreError::InvalidArgument(
            "XCTest supports at most 256 targets per task".into(),
        )
        .into());
    }
    if let Some(timeout_ms) = spec.timeout_ms
        && !(1_000..=86_400_000).contains(&timeout_ms)
    {
        return Err(
            CoreError::InvalidArgument("XCTest timeout must be 1000..86400000 ms".into()).into(),
        );
    }
    for target in &spec.targets {
        if target.name.trim().is_empty() {
            return Err(
                CoreError::InvalidArgument("XCTest target name cannot be empty".into()).into(),
            );
        }
        if target.runner_bundle_id.trim().is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "XCTest target {:?} runner bundle id cannot be empty",
                target.name
            ))
            .into());
        }
        if !target.is_ui_test {
            if target
                .xctest_config
                .as_deref()
                .and_then(|name| name.strip_suffix(".xctest"))
                .is_none_or(str::is_empty)
            {
                return Err(CoreError::InvalidArgument(format!(
                    "unit XCTest target {:?} requires an explicit non-empty .xctest bundle name",
                    target.name
                ))
                .into());
            }
            if target.target_app_name.is_some() || target.target_bundle_id.is_some() {
                return Err(CoreError::InvalidArgument(format!(
                    "unit XCTest target {:?} uses its runner bundle as test host and cannot specify a target app",
                    target.name
                ))
                .into());
            }
        }
        if let Some((key, _)) = target
            .environment
            .iter()
            .find(|(key, _)| key.is_empty() || key.contains('='))
        {
            return Err(CoreError::InvalidArgument(format!(
                "invalid XCTest environment key {key:?}"
            ))
            .into());
        }
    }
    Ok(())
}

fn validate_location_simulation_spec(
    spec: &LocationSimulationSpec,
) -> Result<(), TaskManagerError> {
    if spec.source_path.trim().is_empty() || !Path::new(&spec.source_path).is_absolute() {
        return Err(CoreError::InvalidArgument(
            "location simulation source path must be an absolute GPX path".into(),
        )
        .into());
    }
    if spec.points.is_empty() {
        return Err(CoreError::InvalidArgument(
            "location simulation requires at least one GPX track point".into(),
        )
        .into());
    }
    if spec.points.len() > 10_000 {
        return Err(CoreError::InvalidArgument(
            "location simulation supports at most 10000 GPX track points".into(),
        )
        .into());
    }
    for (index, point) in spec.points.iter().enumerate() {
        if !point.latitude.is_finite() || !(-90.0..=90.0).contains(&point.latitude) {
            return Err(CoreError::InvalidArgument(format!(
                "location simulation point {} latitude must be -90..90",
                index + 1
            ))
            .into());
        }
        if !point.longitude.is_finite() || !(-180.0..=180.0).contains(&point.longitude) {
            return Err(CoreError::InvalidArgument(format!(
                "location simulation point {} longitude must be -180..180",
                index + 1
            ))
            .into());
        }
    }
    Ok(())
}

fn validate_ui_runner_spec(spec: &UiRunnerSpec) -> Result<(), TaskManagerError> {
    let (device_port, health_path) = match spec.target {
        UiRunnerTarget::Wda => (8100, "/status"),
        UiRunnerTarget::DeviceKit => (12004, "/health"),
    };
    if spec.device_port != device_port || spec.health_path != health_path {
        return Err(CoreError::InvalidArgument(format!(
            "{} UI runner requires device port {device_port} and health path {health_path}",
            spec.target.as_str()
        ))
        .into());
    }
    for (name, value) in [
        ("runner bundle id", spec.runner_bundle_id.as_str()),
        ("target bundle id", spec.target_bundle_id.as_str()),
        ("xctest config", spec.xctest_config.as_str()),
        ("bind host", spec.bind_host.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(
                CoreError::InvalidArgument(format!("UI runner {name} cannot be empty")).into(),
            );
        }
    }
    if !spec.xctest_config.ends_with(".xctest") {
        return Err(CoreError::InvalidArgument(
            "UI runner xctest config must end with .xctest".into(),
        )
        .into());
    }
    if !(1_000..=300_000).contains(&spec.readiness_timeout_ms) {
        return Err(CoreError::InvalidArgument(
            "UI runner readiness timeout must be 1000..300000 ms".into(),
        )
        .into());
    }
    Ok(())
}

fn validate_app_launch_spec(spec: &AppLaunchSpec) -> Result<(), TaskManagerError> {
    if spec.bundle_id.trim().is_empty() {
        return Err(CoreError::InvalidArgument("launch requires a bundle ID".into()).into());
    }
    if spec.bundle_id.contains(char::is_whitespace) {
        return Err(CoreError::InvalidArgument(
            "launch bundle ID cannot contain whitespace".into(),
        )
        .into());
    }
    if spec.arguments.len() > 4096 {
        return Err(
            CoreError::InvalidArgument("launch supports at most 4096 arguments".into()).into(),
        );
    }
    if let Some((key, _)) = spec
        .environment
        .iter()
        .find(|(key, _)| key.is_empty() || key.contains('='))
    {
        return Err(
            CoreError::InvalidArgument(format!("invalid launch environment key {key:?}")).into(),
        );
    }
    Ok(())
}

fn validate_webinspector_js_shell_spec(
    spec: &WebInspectorJsShellSpec,
) -> Result<(), TaskManagerError> {
    if !(1_000..=120_000).contains(&spec.command_timeout_ms) {
        return Err(CoreError::InvalidArgument(
            "WebInspector JS shell timeout must be 1000..120000 ms".into(),
        )
        .into());
    }
    if spec
        .url
        .as_deref()
        .is_some_and(|value| value.trim().is_empty() || value.len() > 64 * 1024)
    {
        return Err(CoreError::InvalidArgument(
            "WebInspector JS shell URL must be non-empty and at most 64 KiB".into(),
        )
        .into());
    }
    if spec
        .bundle_id
        .as_deref()
        .is_some_and(|value| value.trim().is_empty() || value.len() > 1024)
    {
        return Err(CoreError::InvalidArgument(
            "WebInspector JS shell bundle ID must be non-empty and at most 1024 bytes".into(),
        )
        .into());
    }
    Ok(())
}

fn device_selectors_overlap(left: &Option<String>, right: &Option<String>) -> bool {
    left.is_none() || right.is_none() || left == right
}

fn socket_addresses_overlap(left: std::net::SocketAddr, right: std::net::SocketAddr) -> bool {
    left.port() == right.port()
        && (left.ip() == right.ip()
            || (left.is_ipv4() == right.is_ipv4()
                && (left.ip().is_unspecified() || right.ip().is_unspecified())))
}

fn listening_endpoint(spec: &TaskSpec) -> Option<(&str, u16)> {
    match spec {
        TaskSpec::Forward(spec) => Some((&spec.bind_host, spec.host_port)),
        TaskSpec::Tunnel(spec) => Some((&spec.userspace_bind_host, spec.userspace_port)),
        TaskSpec::ScreenshotStream(spec) => Some((&spec.bind_host, spec.port)),
        TaskSpec::WebInspectorCdp(spec) => Some((&spec.bind_host, spec.port)),
        TaskSpec::DebugProxy(spec) => Some((&spec.bind_host, spec.port)),
        TaskSpec::Dproxy(spec) => Some((&spec.bind_host, spec.port)),
        TaskSpec::UiRunner(spec) => Some((&spec.bind_host, spec.host_port)),
        _ => None,
    }
}

fn task_output_path(spec: &TaskSpec) -> Option<&str> {
    match spec {
        TaskSpec::Stream(spec) => spec.output_path.as_deref(),
        TaskSpec::AccessibilityInspector(spec) => spec.output_path.as_deref(),
        TaskSpec::LocationSimulation(spec) => spec.output_path.as_deref(),
        TaskSpec::UiStream(spec) => spec.output_path.as_deref(),
        TaskSpec::UiRunner(spec) => spec.output_path.as_deref(),
        TaskSpec::Wda(spec) => spec.output_path.as_deref(),
        TaskSpec::XCTest(spec) => spec.output_path.as_deref(),
        TaskSpec::AppLaunch(spec) => spec.output_path.as_deref(),
        TaskSpec::WebInspectorJsShell(spec) => spec.output_path.as_deref(),
        TaskSpec::Dproxy(spec) => spec.output_path.as_deref(),
        _ => None,
    }
}

fn normalize_ui_base_url(value: &str, backend: &str) -> Result<String, TaskManagerError> {
    let url = reqwest::Url::parse(value).map_err(|error| {
        CoreError::InvalidArgument(format!("{backend} UI stream URL is invalid: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(CoreError::InvalidArgument(format!(
            "{backend} UI stream URL must be an absolute http(s) URL"
        ))
        .into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::InvalidArgument(format!(
            "{backend} UI stream URL must not contain credentials"
        ))
        .into());
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(CoreError::InvalidArgument(format!(
            "{backend} UI stream URL must not contain a query or fragment"
        ))
        .into());
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

async fn stop_runtime(task: &ManagedTask) {
    let mut runtime = task.runtime.lock().await;
    runtime.cancel.cancel();
    if let Some(join) = runtime.join.take() {
        join_or_abort(join).await;
    }
}

async fn join_or_abort(mut join: JoinHandle<()>) {
    if timeout(Duration::from_secs(3), &mut join).await.is_err() {
        join.abort();
        let _ = join.await;
    }
}

fn selector_matches(selector: &TaskSelector, record: &TaskRecord) -> bool {
    match selector {
        TaskSelector::Id(id) => record.id == *id,
        TaskSelector::ForwardPort(port) => matches!(
            &record.spec, TaskSpec::Forward(spec) if spec.host_port == *port
        ),
        TaskSelector::TunnelUdid(udid) => matches!(
            &record.spec,
            TaskSpec::Tunnel(spec) if spec.udid.as_deref() == Some(udid.as_str())
        ),
        TaskSelector::Kind(kind) => record.kind == *kind,
        TaskSelector::All => true,
    }
}

async fn wait_until_started(record: &RwLock<TaskRecord>, maximum: Duration) {
    let deadline = tokio::time::Instant::now() + maximum;
    while tokio::time::Instant::now() < deadline {
        if record.read().await.runtime_state != RuntimeState::Starting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn set_state(
    record: &RwLock<TaskRecord>,
    state: RuntimeState,
    message: Option<String>,
    runtime: Value,
) {
    let mut record = record.write().await;
    record.runtime_state = state;
    record.updated_at_ms = unix_time_ms();
    record.message = message;
    record.runtime = runtime;
    if record.runtime_state == RuntimeState::Running && record.started_at_ms.is_none() {
        record.started_at_ms = Some(record.updated_at_ms);
    }
}

async fn increment_attempt(record: &RwLock<TaskRecord>) -> u32 {
    let mut record = record.write().await;
    record.attempts = record.attempts.saturating_add(1);
    record.updated_at_ms = unix_time_ms();
    record.attempts
}

fn retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_millis((250_u64 << shift).min(5_000))
}

async fn wait_retry(cancel: &CancellationToken, attempt: u32) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(retry_delay(attempt)) => true,
    }
}

async fn mark_cancelled(record: &RwLock<TaskRecord>) {
    set_state(
        record,
        RuntimeState::Stopped,
        Some("server stopped task runtime".into()),
        Value::Null,
    )
    .await;
}

fn stream_content_type(spec: &StreamTaskSpec) -> &'static str {
    match spec.kind {
        TaskKind::Pcap => "application/vnd.tcpdump.pcap",
        TaskKind::Syslog | TaskKind::OsTrace | TaskKind::Notifications | TaskKind::Sysmontap
            if spec.format == StreamFormat::JsonLines =>
        {
            "application/x-ndjson"
        }
        _ => "text/plain; charset=utf-8",
    }
}

const fn ui_stream_content_type(format: UiStreamFormat) -> &'static str {
    match format {
        UiStreamFormat::Mjpeg => "multipart/x-mixed-replace",
        UiStreamFormat::H264 => "video/h264",
    }
}

fn stream_error_state(error: &CoreError) -> RuntimeState {
    if matches!(
        error,
        CoreError::DeviceNotFound(_)
            | CoreError::DeviceUnavailable(_)
            | CoreError::UsbmuxdUnavailable(_)
    ) {
        RuntimeState::Waiting
    } else if error.retryable()
        && !matches!(
            error,
            CoreError::InvalidArgument(_)
                | CoreError::NotImplemented(_)
                | CoreError::UnsupportedPlatform(_)
                | CoreError::PermissionDenied(_)
        )
    {
        RuntimeState::BackingOff
    } else {
        RuntimeState::Failed
    }
}

async fn supervise_location_simulation(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: LocationSimulationSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output = PathBuf::from(
        spec.output_path
            .as_deref()
            .expect("normalized location simulation output path"),
    );
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "opening location simulation session (attempt {attempt})"
            )),
            json!({
                "sourcePath": spec.source_path,
                "outputPath": output,
                "pointCount": spec.points.len(),
                "retryInMs": retry_delay(attempt).as_millis(),
            }),
        )
        .await;
        let tunnel =
            match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
                Ok(tunnel) => tunnel.map(|(_, tunnel)| tunnel),
                Err(error) => {
                    let state = stream_error_state(&error);
                    set_state(
                        &record,
                        state.clone(),
                        Some(error.to_string()),
                        json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                    )
                    .await;
                    if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                        if cancel.is_cancelled() {
                            mark_cancelled(&record).await;
                        }
                        return;
                    }
                    continue;
                }
            };
        let start = backend.start_location_simulation(&spec, tunnel);
        tokio::pin!(start);
        let session = tokio::select! {
            () = cancel.cancelled() => {
                mark_cancelled(&record).await;
                return;
            }
            result = &mut start => result,
        };
        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                let state = stream_error_state(&error);
                if state == RuntimeState::Failed {
                    finish_location_simulation(
                        &record,
                        RuntimeState::Failed,
                        error.to_string(),
                        json!({
                            "sourcePath": spec.source_path,
                            "outputPath": output,
                            "pointCount": spec.points.len(),
                            "completedPoints": 0,
                        }),
                    )
                    .await;
                    return;
                }
                set_state(
                    &record,
                    state,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };

        let info = session.info().clone();
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                let _ = session.clear().await;
                let _ = session.close().await;
                finish_location_simulation(
                    &record,
                    RuntimeState::Failed,
                    format!("open location simulation output: {error}"),
                    location_simulation_runtime(&spec, &info, &output, 0, 0),
                )
                .await;
                return;
            }
        };
        let mut bytes_written = 0_u64;
        if let Err(error) = append_location_event(
            &mut file,
            json!({
                "event": "simulation_started",
                "timestampMs": unix_time_ms(),
                "udid": info.udid,
                "serviceTransport": info.service_transport,
                "pointCount": spec.points.len(),
                "sourcePath": spec.source_path,
            }),
            &mut bytes_written,
        )
        .await
        {
            let _ = session.clear().await;
            let _ = session.close().await;
            finish_location_simulation(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                location_simulation_runtime(&spec, &info, &output, 0, bytes_written),
            )
            .await;
            return;
        }
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "running {} GPX track points on {}",
                spec.points.len(),
                info.udid
            )),
            location_simulation_runtime(&spec, &info, &output, 0, bytes_written),
        )
        .await;

        for (index, point) in spec.points.iter().enumerate() {
            if point.delay_before_ms > 0 {
                tokio::select! {
                    () = cancel.cancelled() => {
                        cancel_location_simulation(
                            &record,
                            &mut *session,
                            &mut file,
                            LocationSimulationCancelContext {
                                spec: &spec,
                                info: &info,
                                output: &output,
                                completed_points: index,
                            },
                            &mut bytes_written,
                        ).await;
                        return;
                    }
                    () = tokio::time::sleep(Duration::from_millis(point.delay_before_ms)) => {}
                }
            }
            if cancel.is_cancelled() {
                cancel_location_simulation(
                    &record,
                    &mut *session,
                    &mut file,
                    LocationSimulationCancelContext {
                        spec: &spec,
                        info: &info,
                        output: &output,
                        completed_points: index,
                    },
                    &mut bytes_written,
                )
                .await;
                return;
            }
            if let Err(error) = session.set_location(point.latitude, point.longitude).await {
                let clear_error = session.clear().await.err().map(|error| error.to_string());
                let _ = append_location_event(
                    &mut file,
                    json!({
                        "event": "simulation_failed",
                        "timestampMs": unix_time_ms(),
                        "pointIndex": index,
                        "error": error.to_string(),
                        "clearError": clear_error,
                    }),
                    &mut bytes_written,
                )
                .await;
                let _ = session.close().await;
                finish_location_simulation(
                    &record,
                    RuntimeState::Failed,
                    format!("set GPX track point {}: {error}", index + 1),
                    location_simulation_runtime(&spec, &info, &output, index, bytes_written),
                )
                .await;
                return;
            }
            if let Err(error) = append_location_event(
                &mut file,
                location_point_event(index, point),
                &mut bytes_written,
            )
            .await
            {
                let _ = session.clear().await;
                let _ = session.close().await;
                finish_location_simulation(
                    &record,
                    RuntimeState::Failed,
                    error.to_string(),
                    location_simulation_runtime(&spec, &info, &output, index + 1, bytes_written),
                )
                .await;
                return;
            }
            set_state(
                &record,
                RuntimeState::Running,
                Some(format!(
                    "applied GPX track point {}/{}",
                    index + 1,
                    spec.points.len()
                )),
                location_simulation_runtime(&spec, &info, &output, index + 1, bytes_written),
            )
            .await;
        }

        if let Err(error) = append_location_event(
            &mut file,
            json!({
                "event": "simulation_completed",
                "timestampMs": unix_time_ms(),
                "pointCount": spec.points.len(),
            }),
            &mut bytes_written,
        )
        .await
        {
            let _ = session.clear().await;
            let _ = session.close().await;
            finish_location_simulation(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                location_simulation_runtime(
                    &spec,
                    &info,
                    &output,
                    spec.points.len(),
                    bytes_written,
                ),
            )
            .await;
            return;
        }
        let _ = session.close().await;
        finish_location_simulation(
            &record,
            RuntimeState::Completed,
            format!(
                "location simulation completed {} GPX track points; use resetlocation to clear the final point",
                spec.points.len()
            ),
            location_simulation_runtime(
                &spec,
                &info,
                &output,
                spec.points.len(),
                bytes_written,
            ),
        )
        .await;
        return;
    }
}

struct LocationSimulationCancelContext<'a> {
    spec: &'a LocationSimulationSpec,
    info: &'a LocationSimulationInfo,
    output: &'a Path,
    completed_points: usize,
}

async fn cancel_location_simulation(
    record: &RwLock<TaskRecord>,
    session: &mut dyn rust_ios_core::DeviceLocationSimulation,
    file: &mut tokio::fs::File,
    context: LocationSimulationCancelContext<'_>,
    bytes_written: &mut u64,
) {
    let clear_error = session.clear().await.err().map(|error| error.to_string());
    let _ = append_location_event(
        file,
        json!({
            "event": "simulation_cancelled",
            "timestampMs": unix_time_ms(),
            "completedPoints": context.completed_points,
            "clearError": clear_error,
        }),
        bytes_written,
    )
    .await;
    let _ = session.close().await;
    set_state(
        record,
        RuntimeState::Stopped,
        Some("location simulation cancelled; simulated location was cleared".into()),
        location_simulation_runtime(
            context.spec,
            context.info,
            context.output,
            context.completed_points,
            *bytes_written,
        ),
    )
    .await;
}

fn location_point_event(index: usize, point: &LocationSimulationPoint) -> Value {
    json!({
        "event": "location_set",
        "timestampMs": unix_time_ms(),
        "pointIndex": index,
        "latitude": point.latitude,
        "longitude": point.longitude,
        "delayBeforeMs": point.delay_before_ms,
        "sourceTime": point.source_time,
    })
}

async fn append_location_event(
    file: &mut tokio::fs::File,
    event: Value,
    bytes_written: &mut u64,
) -> CoreResult<()> {
    let mut bytes = serde_json::to_vec(&event)
        .map_err(|error| CoreError::Backend(format!("encode location event: {error}")))?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    file.flush().await?;
    *bytes_written = bytes_written.saturating_add(bytes.len() as u64);
    Ok(())
}

fn location_simulation_runtime(
    spec: &LocationSimulationSpec,
    info: &LocationSimulationInfo,
    output: &Path,
    completed_points: usize,
    bytes_written: u64,
) -> Value {
    json!({
        "udid": info.udid,
        "serviceTransport": info.service_transport,
        "sourcePath": spec.source_path,
        "outputPath": output,
        "contentType": "application/x-ndjson",
        "pointCount": spec.points.len(),
        "completedPoints": completed_points,
        "bytesWritten": bytes_written,
    })
}

async fn finish_location_simulation(
    record: &RwLock<TaskRecord>,
    state: RuntimeState,
    message: String,
    runtime: Value,
) {
    let mut record = record.write().await;
    record.desired_state = DesiredState::Stopped;
    record.runtime_state = state;
    record.updated_at_ms = unix_time_ms();
    record.message = Some(message);
    record.runtime = runtime;
}

async fn supervise_device_state(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: DeviceStateSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "enabling device condition {} / {} (attempt {attempt})",
                spec.condition_identifier, spec.profile_identifier
            )),
            Value::Null,
        )
        .await;
        let condition = match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref())
            .await
            .map(|tunnel| tunnel.map(|(_, tunnel)| tunnel))
        {
            Ok(tunnel) => backend.start_device_condition(&spec, tunnel).await,
            Err(error) => Err(error),
        };
        let condition = match condition {
            Ok(condition) => condition,
            Err(error) => {
                let state = stream_error_state(&error);
                set_state(
                    &record,
                    state.clone(),
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let info = condition.info().clone();
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "device condition {} / {} is active",
                info.condition_identifier, info.profile_identifier
            )),
            serde_json::to_value(&info).unwrap_or(Value::Null),
        )
        .await;
        let task_id = record.read().await.id;
        info!(
            %task_id,
            udid = %info.udid,
            condition = %info.condition_identifier,
            profile = %info.profile_identifier,
            "device condition task running"
        );

        cancel.cancelled().await;
        match condition.close().await {
            Ok(()) => mark_cancelled(&record).await,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::Stopped,
                    Some(format!("device condition stop failed: {error}")),
                    Value::Null,
                )
                .await;
            }
        }
        return;
    }
}

#[derive(Debug, Clone)]
enum ManagedStreamSpec {
    Stream(StreamTaskSpec),
    AccessibilityInspector(AccessibilityInspectorSpec),
}

impl ManagedStreamSpec {
    fn kind(&self) -> TaskKind {
        match self {
            Self::Stream(spec) => spec.kind.clone(),
            Self::AccessibilityInspector(_) => TaskKind::AccessibilityInspector,
        }
    }

    fn output_path(&self) -> Option<&str> {
        match self {
            Self::Stream(spec) => spec.output_path.as_deref(),
            Self::AccessibilityInspector(spec) => spec.output_path.as_deref(),
        }
    }

    async fn start(
        &self,
        backend: &Arc<dyn DeviceBackend>,
        active_tunnels: &Arc<RwLock<HashMap<String, ActiveTunnel>>>,
    ) -> CoreResult<Box<dyn rust_ios_core::DeviceStream>> {
        match self {
            Self::Stream(spec) => {
                let tunnel = if stream_kind_uses_dvt(&spec.kind) {
                    lookup_optional_active_tunnel(active_tunnels, spec.udid.as_deref())
                        .await?
                        .map(|(_, tunnel)| tunnel)
                } else {
                    None
                };
                backend.start_stream(spec, tunnel).await
            }
            Self::AccessibilityInspector(spec) => backend.start_accessibility_inspector(spec).await,
        }
    }
}

async fn supervise_stream(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: ManagedStreamSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output = PathBuf::from(spec.output_path().unwrap_or(""));
    let kind = spec.kind();
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "opening {} stream (attempt {attempt})",
                kind.as_str()
            )),
            json!({ "outputPath": output }),
        )
        .await;

        let stream = spec.start(&backend, &active_tunnels).await;
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                let state = stream_error_state(&error);
                set_state(
                    &record,
                    state.clone(),
                    Some(error.to_string()),
                    json!({
                        "outputPath": output,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };

        let info = stream.info().clone();
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                let error = CoreError::Io(error);
                let _ = stream.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({
                        "outputPath": output,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let mut bytes_written = file.metadata().await.map_or(0, |metadata| metadata.len());
        if bytes_written == 0 && !info.initial_bytes.is_empty() {
            if let Err(error) = file.write_all(&info.initial_bytes).await {
                let _ = stream.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(CoreError::Io(error).to_string()),
                    json!({ "outputPath": output }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
            bytes_written = info.initial_bytes.len() as u64;
        }
        if let Err(error) = file.flush().await {
            let _ = stream.close().await;
            set_state(
                &record,
                RuntimeState::BackingOff,
                Some(CoreError::Io(error).to_string()),
                json!({ "outputPath": output }),
            )
            .await;
            if !wait_retry(&cancel, attempt).await {
                mark_cancelled(&record).await;
                return;
            }
            continue;
        }

        let mut chunks = 0_u64;
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!("capturing {} stream", kind.as_str())),
            json!({
                "outputPath": output,
                "contentType": info.content_type,
                "serviceTransport": info.service_transport,
                "bytesWritten": bytes_written,
                "chunksWritten": chunks,
            }),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, kind = kind.as_str(), path = %output.display(), "stream task running");

        let failure = loop {
            tokio::select! {
                () = cancel.cancelled() => break None,
                chunk = stream.next_chunk() => {
                    match chunk {
                        Ok(chunk) => {
                            if chunk.is_empty() {
                                continue;
                            }
                            if let Err(error) = file.write_all(&chunk).await {
                                break Some(CoreError::Io(error));
                            }
                            if let Err(error) = file.flush().await {
                                break Some(CoreError::Io(error));
                            }
                            bytes_written = bytes_written.saturating_add(chunk.len() as u64);
                            chunks = chunks.saturating_add(1);
                            set_state(
                                &record,
                                RuntimeState::Running,
                                Some(format!("capturing {} stream", kind.as_str())),
                                json!({
                                    "outputPath": output,
                                    "contentType": info.content_type,
                                    "serviceTransport": info.service_transport,
                                    "bytesWritten": bytes_written,
                                    "chunksWritten": chunks,
                                }),
                            )
                            .await;
                        }
                        Err(error) => break Some(error),
                    }
                }
            }
        };
        let _ = stream.close().await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        if let Some(error) = failure {
            warn!(%task_id, %error, "stream task failed");
            let state = stream_error_state(&error);
            set_state(
                &record,
                state.clone(),
                Some(error.to_string()),
                json!({
                    "outputPath": output,
                    "contentType": info.content_type,
                    "serviceTransport": info.service_transport,
                    "bytesWritten": bytes_written,
                    "chunksWritten": chunks,
                    "retryInMs": retry_delay(attempt).as_millis(),
                }),
            )
            .await;
            if state == RuntimeState::Failed {
                return;
            }
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn supervise_ui_stream(
    record: Arc<RwLock<TaskRecord>>,
    spec: UiStreamSpec,
    cancel: CancellationToken,
    manager: Weak<TaskManager>,
) {
    let output = PathBuf::from(spec.output_path.as_deref().unwrap_or(""));
    let http = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(http) => http,
        Err(error) => {
            set_state(
                &record,
                RuntimeState::Failed,
                Some(format!("build UI stream HTTP client: {error}")),
                json!({ "outputPath": output }),
            )
            .await;
            return;
        }
    };

    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("opening UI stream (attempt {attempt})")),
            json!({
                "outputPath": output,
                "requestedDriver": spec.driver.as_str(),
                "format": spec.format.as_str(),
            }),
        )
        .await;

        let opened = tokio::select! {
            () = cancel.cancelled() => {
                mark_cancelled(&record).await;
                return;
            }
            opened = open_ui_stream(&http, &spec, &manager) => opened,
        };
        let (resolved_driver, stream_url, mut response) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                let state = stream_error_state(&error);
                set_state(
                    &record,
                    state.clone(),
                    Some(error.to_string()),
                    json!({
                        "outputPath": output,
                        "requestedDriver": spec.driver.as_str(),
                        "format": spec.format.as_str(),
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| ui_stream_content_type(spec.format).into());
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(CoreError::Io(error).to_string()),
                    json!({
                        "outputPath": output,
                        "requestedDriver": spec.driver.as_str(),
                        "resolvedDriver": resolved_driver.as_str(),
                        "format": spec.format.as_str(),
                        "streamUrl": stream_url.as_str(),
                    }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let mut bytes_written = file.metadata().await.map_or(0, |metadata| metadata.len());
        let mut chunks_written = 0_u64;
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "capturing {} {} UI stream",
                resolved_driver.as_str(),
                spec.format.as_str()
            )),
            ui_stream_runtime(
                &spec,
                resolved_driver,
                &stream_url,
                &output,
                &content_type,
                bytes_written,
                chunks_written,
            ),
        )
        .await;
        let task_id = record.read().await.id;
        info!(
            %task_id,
            driver = resolved_driver.as_str(),
            format = spec.format.as_str(),
            url = %stream_url,
            path = %output.display(),
            "UI stream task running"
        );

        let failure = loop {
            tokio::select! {
                () = cancel.cancelled() => break None,
                chunk = response.chunk() => {
                    match chunk {
                        Ok(Some(chunk)) if chunk.is_empty() => {}
                        Ok(Some(chunk)) => {
                            if let Err(error) = file.write_all(&chunk).await {
                                break Some(CoreError::Io(error));
                            }
                            if let Err(error) = file.flush().await {
                                break Some(CoreError::Io(error));
                            }
                            bytes_written = bytes_written.saturating_add(chunk.len() as u64);
                            chunks_written = chunks_written.saturating_add(1);
                            set_state(
                                &record,
                                RuntimeState::Running,
                                Some(format!(
                                    "capturing {} {} UI stream",
                                    resolved_driver.as_str(),
                                    spec.format.as_str()
                                )),
                                ui_stream_runtime(
                                    &spec,
                                    resolved_driver,
                                    &stream_url,
                                    &output,
                                    &content_type,
                                    bytes_written,
                                    chunks_written,
                                ),
                            )
                            .await;
                        }
                        Ok(None) => break Some(CoreError::DeviceUnavailable(
                            "UI stream ended unexpectedly".into(),
                        )),
                        Err(error) => break Some(CoreError::DeviceUnavailable(format!(
                            "read UI stream response: {error}"
                        ))),
                    }
                }
            }
        };
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        if let Some(error) = failure {
            warn!(%task_id, %error, "UI stream task failed");
            let state = stream_error_state(&error);
            set_state(&record, state.clone(), Some(error.to_string()), {
                let mut runtime = ui_stream_runtime(
                    &spec,
                    resolved_driver,
                    &stream_url,
                    &output,
                    &content_type,
                    bytes_written,
                    chunks_written,
                );
                runtime["retryInMs"] = json!(retry_delay(attempt).as_millis());
                runtime
            })
            .await;
            if state == RuntimeState::Failed {
                return;
            }
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn open_ui_stream(
    http: &reqwest::Client,
    spec: &UiStreamSpec,
    manager: &Weak<TaskManager>,
) -> CoreResult<(UiAutomationDriver, reqwest::Url, reqwest::Response)> {
    let mut wda_urls = None;
    let driver = match spec.driver {
        UiAutomationDriver::Wda => {
            wda_urls = Some(resolve_wda_stream_urls(spec, manager).await?);
            UiAutomationDriver::Wda
        }
        UiAutomationDriver::DeviceKit => UiAutomationDriver::DeviceKit,
        UiAutomationDriver::Auto => {
            if ui_backend_healthy(http, &spec.devicekit_url, "/health").await {
                UiAutomationDriver::DeviceKit
            } else {
                if spec.format != UiStreamFormat::Mjpeg {
                    return Err(CoreError::DeviceUnavailable(
                        "no compatible UI stream backend is reachable".into(),
                    ));
                }
                let resolved = resolve_wda_stream_urls(spec, manager).await?;
                if !ui_backend_healthy(http, &resolved.0, "/status").await {
                    return Err(CoreError::DeviceUnavailable(
                        "no compatible UI stream backend is reachable".into(),
                    ));
                }
                wda_urls = Some(resolved);
                UiAutomationDriver::Wda
            }
        }
    };
    if driver == UiAutomationDriver::Wda && spec.format == UiStreamFormat::H264 {
        return Err(CoreError::InvalidArgument(
            "WDA UI stream supports MJPEG only".into(),
        ));
    }
    let raw_url = match driver {
        UiAutomationDriver::Wda => {
            let (wda_url, wda_stream_url) = wda_urls.expect("WDA URLs were resolved");
            wda_stream_url.unwrap_or_else(|| format!("{wda_url}/{}", spec.format.as_str()))
        }
        UiAutomationDriver::DeviceKit => {
            format!("{}/{}", spec.devicekit_url, spec.format.as_str())
        }
        UiAutomationDriver::Auto => unreachable!(),
    };
    let mut url = reqwest::Url::parse(&raw_url)
        .map_err(|error| CoreError::InvalidArgument(format!("build UI stream URL: {error}")))?;
    {
        let mut query = url.query_pairs_mut();
        if let Some(value) = spec.fps {
            query.append_pair("fps", &value.to_string());
        }
        if let Some(value) = spec.quality {
            query.append_pair("quality", &value.to_string());
        }
        if let Some(value) = spec.scale {
            query.append_pair("scale", &value.to_string());
        }
        if let Some(value) = spec.bitrate {
            query.append_pair("bitrate", &value.to_string());
        }
    }
    let response = timeout(
        Duration::from_secs(10),
        http.get(url.clone())
            .header(reqwest::header::ACCEPT, ui_stream_content_type(spec.format))
            .send(),
    )
    .await
    .map_err(|_| CoreError::DeviceUnavailable(format!("open UI stream {url} timed out")))?
    .map_err(|error| CoreError::DeviceUnavailable(format!("open UI stream {url}: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = timeout(Duration::from_secs(2), response.bytes())
            .await
            .ok()
            .and_then(Result::ok)
            .map(|body| String::from_utf8_lossy(&body[..body.len().min(512)]).into_owned())
            .unwrap_or_default();
        return Err(CoreError::DeviceUnavailable(format!(
            "UI stream backend returned HTTP {status}: {body}"
        )));
    }
    Ok((driver, url, response))
}

async fn resolve_wda_stream_urls(
    spec: &UiStreamSpec,
    manager: &Weak<TaskManager>,
) -> CoreResult<(String, Option<String>)> {
    let Some(task_id) = spec.wda_task_id else {
        return Ok((spec.wda_url.clone(), None));
    };
    let manager = manager.upgrade().ok_or_else(|| {
        CoreError::DeviceUnavailable("UI stream task manager is shutting down".into())
    })?;
    let task = manager.show(task_id).await.map_err(|error| {
        CoreError::DeviceUnavailable(format!(
            "referenced WDA task {task_id} is unavailable: {error}"
        ))
    })?;
    if task.runtime_state != RuntimeState::Running {
        return Err(CoreError::DeviceUnavailable(format!(
            "referenced WDA task {task_id} is {:?}",
            task.runtime_state
        )));
    }
    let wda_url = task.runtime["wdaUrl"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "referenced WDA task {task_id} has no runtime wdaUrl"
            ))
        })?;
    let stream_url = task.runtime["mjpegUrl"].as_str().map(str::to_owned);
    Ok((wda_url, stream_url))
}

async fn ui_backend_healthy(http: &reqwest::Client, base_url: &str, path: &str) -> bool {
    timeout(
        Duration::from_secs(5),
        http.get(format!("{base_url}{path}")).send(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .is_some_and(|response| response.status().is_success())
}

fn ui_stream_runtime(
    spec: &UiStreamSpec,
    resolved_driver: UiAutomationDriver,
    stream_url: &reqwest::Url,
    output: &Path,
    content_type: &str,
    bytes_written: u64,
    chunks_written: u64,
) -> Value {
    json!({
        "outputPath": output,
        "requestedDriver": spec.driver.as_str(),
        "resolvedDriver": resolved_driver.as_str(),
        "wdaTaskId": spec.wda_task_id,
        "format": spec.format.as_str(),
        "streamUrl": stream_url.as_str(),
        "contentType": content_type,
        "transport": "http",
        "bytesWritten": bytes_written,
        "chunksWritten": chunks_written,
    })
}

async fn supervise_screenshot_stream(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: ScreenshotStreamSpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("starting screenshot stream (attempt {attempt})")),
            json!({ "bindHost": spec.bind_host, "port": spec.port }),
        )
        .await;
        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let tunnel = lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref())
            .await
            .map(|tunnel| tunnel.map(|(_, tunnel)| tunnel));
        let stream_spec = StreamTaskSpec {
            kind: TaskKind::ScreenshotStream,
            udid: spec.udid.clone(),
            output_path: None,
            format: StreamFormat::Raw,
            pid: None,
            process: None,
            os_trace: None,
            watch_kill_targets: None,
        };
        let stream = match tunnel {
            Ok(tunnel) => backend.start_stream(&stream_spec, tunnel).await,
            Err(error) => Err(error),
        };
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                let state = stream_error_state(&error);
                set_state(
                    &record,
                    state.clone(),
                    Some(error.to_string()),
                    json!({
                        "bindHost": spec.bind_host,
                        "port": spec.port,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let service_transport = stream.info().service_transport.clone();
        let endpoint = match listener.local_addr() {
            Ok(address) => address,
            Err(error) => {
                let _ = stream.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(CoreError::Io(error).to_string()),
                    Value::Null,
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let (frames, _) = broadcast::channel::<Arc<Vec<u8>>>(2);
        let mut clients = JoinSet::new();
        let mut clients_accepted = 0_u64;
        let mut frames_captured = 0_u64;
        set_state(
            &record,
            RuntimeState::Running,
            Some("screenshot MJPEG server is running".into()),
            json!({
                "endpoint": endpoint,
                "url": format!("http://{endpoint}/"),
                "contentType": "multipart/x-mixed-replace; boundary=BoundaryString",
                "serviceTransport": service_transport,
                "clientsAccepted": clients_accepted,
                "framesCaptured": frames_captured,
            }),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, %endpoint, "screenshot stream task running");
        let mut ticker = tokio::time::interval(Duration::from_millis(spec.interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let failure = loop {
            tokio::select! {
                () = cancel.cancelled() => break None,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((socket, _)) => {
                            clients_accepted = clients_accepted.saturating_add(1);
                            clients.spawn(serve_mjpeg_client(socket, frames.subscribe()));
                        }
                        Err(error) => break Some(CoreError::Io(error)),
                    }
                }
                _ = ticker.tick() => {
                    match stream.next_chunk().await {
                        Ok(frame) if frame.is_empty() => {}
                        Ok(frame) => {
                            frames_captured = frames_captured.saturating_add(1);
                            let _ = frames.send(Arc::new(frame));
                            if frames_captured.is_multiple_of(10) {
                                set_state(
                                    &record,
                                    RuntimeState::Running,
                                    Some("screenshot MJPEG server is running".into()),
                                    json!({
                                        "endpoint": endpoint,
                                        "url": format!("http://{endpoint}/"),
                                        "contentType": "multipart/x-mixed-replace; boundary=BoundaryString",
                                        "serviceTransport": service_transport,
                                        "clientsAccepted": clients_accepted,
                                        "framesCaptured": frames_captured,
                                    }),
                                ).await;
                            }
                        }
                        Err(error) => break Some(error),
                    }
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result {
                        debug!(%task_id, %error, "screenshot HTTP client task failed");
                    }
                }
            }
        };
        let _ = stream.close().await;
        drop(frames);
        clients.abort_all();
        while clients.join_next().await.is_some() {}
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        if let Some(error) = failure {
            let state = stream_error_state(&error);
            set_state(
                &record,
                state.clone(),
                Some(error.to_string()),
                json!({
                    "endpoint": endpoint,
                    "clientsAccepted": clients_accepted,
                    "framesCaptured": frames_captured,
                    "retryInMs": retry_delay(attempt).as_millis(),
                }),
            )
            .await;
            if state == RuntimeState::Failed {
                return;
            }
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn serve_mjpeg_client(mut socket: TcpStream, mut frames: broadcast::Receiver<Arc<Vec<u8>>>) {
    if timeout(Duration::from_secs(5), read_http_request(&mut socket))
        .await
        .is_err()
    {
        return;
    }
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Server: rust-ios-screenshot-mjpeg\r\n",
        "Connection: close\r\n",
        "Content-Type: multipart/x-mixed-replace; boundary=BoundaryString\r\n",
        "Cache-Control: no-cache, private\r\n",
        "Pragma: no-cache\r\n",
        "Access-Control-Allow-Origin: *\r\n\r\n"
    );
    if socket.write_all(response.as_bytes()).await.is_err() {
        return;
    }
    loop {
        let frame = match frames.recv().await {
            Ok(frame) => frame,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let header = format!(
            "--BoundaryString\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            frame.len()
        );
        if socket.write_all(header.as_bytes()).await.is_err()
            || socket.write_all(&frame).await.is_err()
            || socket.write_all(b"\r\n").await.is_err()
            || socket.flush().await.is_err()
        {
            return;
        }
    }
}

async fn read_http_request(socket: &mut TcpStream) -> io::Result<()> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];
    loop {
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP client disconnected before request headers",
            ));
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(());
        }
        if request.len() > 8 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request headers exceed 8 KiB",
            ));
        }
    }
}

async fn supervise_forward(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: ForwardSpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("binding forward listener (attempt {attempt})")),
            Value::Null,
        )
        .await;

        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.host_port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let bound = match listener.local_addr() {
            Ok(bound) => bound,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    Value::Null,
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "forwarding {bound} to device port {}",
                spec.device_port
            )),
            json!({
                "boundAddress": bound.to_string(),
                "hostPort": bound.port(),
                "devicePort": spec.device_port,
            }),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, %bound, device_port = spec.device_port, "forward task running");

        match run_forward_listener(listener, backend.clone(), spec.clone(), cancel.clone()).await {
            Ok(()) if cancel.is_cancelled() => {
                mark_cancelled(&record).await;
                return;
            }
            Ok(()) => {}
            Err(error) => {
                warn!(%task_id, %error, "forward listener failed");
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
            }
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn run_forward_listener(
    listener: TcpListener,
    backend: Arc<dyn DeviceBackend>,
    spec: ForwardSpec,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                return Ok(());
            }
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                let backend = backend.clone();
                let target = DeviceTarget {
                    udid: spec.udid.clone(),
                    connection: spec.connection.clone(),
                };
                let connection_cancel = cancel.child_token();
                let device_port = spec.device_port;
                connections.spawn(async move {
                    match backend.connect_port(&target, device_port).await {
                        Ok(device) => {
                            if let Err(error) = bridge_streams(Box::new(client), device, connection_cancel).await {
                                debug!(%peer, %error, "forward connection ended with error");
                            }
                        }
                        Err(error) => debug!(%peer, %error, "unable to open device forward connection"),
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    debug!(%error, "forward connection task panicked");
                }
            }
        }
    }
}

async fn supervise_tunnel(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: TunnelSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    if spec.mode == TunnelMode::Kernel {
        supervise_kernel_tunnel(record, backend, spec, cancel, active_tunnels).await;
    } else {
        supervise_userspace_tunnel(record, backend, spec, cancel, active_tunnels).await;
    }
}

async fn supervise_userspace_tunnel(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: TunnelSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("establishing userspace tunnel (attempt {attempt})")),
            Value::Null,
        )
        .await;
        let tunnel = match backend.start_userspace_tunnel(&spec).await {
            Ok(tunnel) => tunnel,
            Err(error) => {
                let retryable = error.retryable()
                    && !matches!(
                        error,
                        CoreError::InvalidArgument(_)
                            | CoreError::NotImplemented(_)
                            | CoreError::UnsupportedPlatform(_)
                            | CoreError::PermissionDenied(_)
                    );
                let state = if matches!(
                    &error,
                    CoreError::DeviceNotFound(_)
                        | CoreError::DeviceUnavailable(_)
                        | CoreError::UsbmuxdUnavailable(_)
                ) {
                    RuntimeState::Waiting
                } else if retryable {
                    RuntimeState::BackingOff
                } else {
                    RuntimeState::Failed
                };
                set_state(
                    &record,
                    state,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !retryable || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let tunnel: Arc<dyn UserSpaceTunnel> = Arc::from(tunnel);
        let endpoint =
            match TcpListener::bind((spec.userspace_bind_host.as_str(), spec.userspace_port)).await
            {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    let _ = tunnel.close().await;
                    set_state(
                        &record,
                        RuntimeState::BackingOff,
                        Some(format!("cannot bind userspace tunnel endpoint: {error}")),
                        json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                    )
                    .await;
                    if !wait_retry(&cancel, attempt).await {
                        mark_cancelled(&record).await;
                        return;
                    }
                    continue;
                }
            };
        let endpoint_addr = match endpoint.local_addr() {
            Ok(address) => address,
            Err(error) => {
                let _ = tunnel.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    Value::Null,
                )
                .await;
                continue;
            }
        };
        let tunnel_info = tunnel.info().clone();
        let task_id = record.read().await.id;
        if let Err(error) =
            register_active_tunnel(&active_tunnels, task_id, &tunnel_info.udid, tunnel.clone())
                .await
        {
            let _ = tunnel.close().await;
            set_state(
                &record,
                RuntimeState::Failed,
                Some(error.to_string()),
                Value::Null,
            )
            .await;
            return;
        }
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "userspace tunnel established for {}",
                tunnel_info.udid
            )),
            json!({
                "udid": tunnel_info.udid,
                "address": tunnel_info.server_address,
                "clientAddress": tunnel_info.client_address,
                "netmask": tunnel_info.netmask,
                "mtu": tunnel_info.mtu,
                "rsdPort": tunnel_info.rsd_port,
                "userspace": true,
                "userspaceEndpoint": endpoint_addr.to_string(),
                "userspacePort": endpoint_addr.port(),
            }),
        )
        .await;
        info!(
            %task_id,
            udid = %tunnel_info.udid,
            endpoint = %endpoint_addr,
            "userspace tunnel task running"
        );

        let result = run_tunnel_endpoint(endpoint, tunnel.clone(), cancel.clone()).await;
        unregister_active_tunnel(&active_tunnels, task_id, &tunnel_info.udid).await;
        let _ = tunnel.close().await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        if let Err(error) = result {
            set_state(
                &record,
                RuntimeState::BackingOff,
                Some(error.to_string()),
                json!({ "retryInMs": retry_delay(attempt).as_millis() }),
            )
            .await;
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn supervise_kernel_tunnel(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: TunnelSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "establishing Windows kernel tunnel (attempt {attempt})"
            )),
            Value::Null,
        )
        .await;
        let mut handle = match backend.start_kernel_tunnel(&spec).await {
            Ok(handle) => handle,
            Err(error) => {
                let retryable = error.retryable()
                    && !matches!(
                        error,
                        CoreError::InvalidArgument(_)
                            | CoreError::NotImplemented(_)
                            | CoreError::UnsupportedPlatform(_)
                            | CoreError::PermissionDenied(_)
                    );
                let state = if matches!(
                    &error,
                    CoreError::DeviceNotFound(_)
                        | CoreError::DeviceUnavailable(_)
                        | CoreError::UsbmuxdUnavailable(_)
                ) {
                    RuntimeState::Waiting
                } else if retryable {
                    RuntimeState::BackingOff
                } else {
                    RuntimeState::Failed
                };
                set_state(
                    &record,
                    state,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !retryable || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let task_id = record.read().await.id;
        let tunnel_info = handle.info.clone();
        if let Err(error) = register_active_tunnel(
            &active_tunnels,
            task_id,
            &tunnel_info.udid,
            handle.route.clone(),
        )
        .await
        {
            let _ = handle.runner.close().await;
            let _ = handle.route.close().await;
            set_state(
                &record,
                RuntimeState::Failed,
                Some(error.to_string()),
                Value::Null,
            )
            .await;
            return;
        }
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "Windows kernel tunnel established for {}",
                tunnel_info.udid
            )),
            json!({
                "udid": tunnel_info.udid,
                "address": tunnel_info.server_address,
                "clientAddress": tunnel_info.client_address,
                "netmask": tunnel_info.netmask,
                "mtu": tunnel_info.mtu,
                "rsdPort": tunnel_info.rsd_port,
                "kernel": true,
                "userspace": false,
                "interfaceName": tunnel_info.interface_name,
            }),
        )
        .await;
        info!(
            %task_id,
            udid = %tunnel_info.udid,
            interface = %tunnel_info.interface_name,
            "Windows kernel tunnel task running"
        );

        let result = handle.runner.run(cancel.clone()).await;
        unregister_active_tunnel(&active_tunnels, task_id, &tunnel_info.udid).await;
        let _ = handle.runner.close().await;
        let _ = handle.route.close().await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        if let Err(error) = result {
            set_state(
                &record,
                RuntimeState::BackingOff,
                Some(error.to_string()),
                json!({ "retryInMs": retry_delay(attempt).as_millis() }),
            )
            .await;
        }
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn register_active_tunnel(
    active_tunnels: &RwLock<HashMap<String, ActiveTunnel>>,
    task_id: Uuid,
    udid: &str,
    tunnel: Arc<dyn UserSpaceTunnel>,
) -> Result<(), CoreError> {
    let mut active = active_tunnels.write().await;
    if let Some(existing) = active.get(udid)
        && existing.task_id != task_id
    {
        return Err(CoreError::InvalidArgument(format!(
            "device {udid} already has a running tunnel task {}",
            existing.task_id
        )));
    }
    active.insert(udid.to_owned(), ActiveTunnel { task_id, tunnel });
    Ok(())
}

async fn unregister_active_tunnel(
    active_tunnels: &RwLock<HashMap<String, ActiveTunnel>>,
    task_id: Uuid,
    udid: &str,
) {
    let mut active = active_tunnels.write().await;
    if active
        .get(udid)
        .is_some_and(|entry| entry.task_id == task_id)
    {
        active.remove(udid);
    }
}

async fn lookup_active_tunnel(
    active_tunnels: &RwLock<HashMap<String, ActiveTunnel>>,
    udid: Option<&str>,
) -> Result<(String, Arc<dyn UserSpaceTunnel>), CoreError> {
    let tunnels = active_tunnels.read().await;
    if let Some(udid) = udid {
        return tunnels
            .get(udid)
            .map(|active| (udid.to_owned(), active.tunnel.clone()))
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!(
                    "no running userspace tunnel for {udid}; run tunnel start --userspace"
                ))
            });
    }
    match tunnels.len() {
        0 => Err(CoreError::DeviceUnavailable(
            "no running userspace tunnel; run tunnel start --userspace".into(),
        )),
        1 => {
            let (udid, active) = tunnels.iter().next().expect("one tunnel");
            Ok((udid.clone(), active.tunnel.clone()))
        }
        _ => Err(CoreError::InvalidArgument(
            "multiple userspace tunnels are running; select one with --udid".into(),
        )),
    }
}

async fn lookup_optional_active_tunnel(
    active_tunnels: &RwLock<HashMap<String, ActiveTunnel>>,
    udid: Option<&str>,
) -> Result<Option<(String, Arc<dyn UserSpaceTunnel>)>, CoreError> {
    let tunnels = active_tunnels.read().await;
    if let Some(udid) = udid {
        return Ok(tunnels
            .get(udid)
            .map(|active| (udid.to_owned(), active.tunnel.clone())));
    }
    match tunnels.len() {
        0 => Ok(None),
        1 => {
            let (udid, active) = tunnels.iter().next().expect("one tunnel");
            Ok(Some((udid.clone(), active.tunnel.clone())))
        }
        _ => Err(CoreError::InvalidArgument(
            "multiple userspace tunnels are running; select one with --udid".into(),
        )),
    }
}

fn stream_kind_uses_dvt(kind: &TaskKind) -> bool {
    matches!(
        kind,
        TaskKind::OsTrace
            | TaskKind::Notifications
            | TaskKind::Sysmontap
            | TaskKind::ScreenshotStream
    )
}

fn command_uses_shared_rsd(invocation: &CommandInvocation) -> bool {
    match invocation.argv.first().map(String::as_str) {
        Some("file") => invocation.argv.iter().skip(2).any(|value| {
            matches!(
                value.as_str(),
                "--app" | "--app-group" | "--crash" | "--temp"
            ) || value.starts_with("--app=")
                || value.starts_with("--app-group=")
        }),
        Some("info") => invocation.argv.get(1).map(String::as_str) == Some("display"),
        Some(
            "ps" | "launch" | "kill" | "memlimitoff" | "devicestate" | "webinspector"
            | "screenshot" | "setlocation" | "resetlocation" | "pasteboard"
            // image：cryptexd 挂载路径需要隧道 RSD（同步 go-ios ca2c56d）。
            // hid：CoreDevice hid.indigo 需要隧道 RSD（同步 go-ios c584042）。
            | "image" | "hid",
        ) => true,
        _ => false,
    }
}

async fn supervise_ui_runner(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: UiRunnerSpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output = PathBuf::from(
        spec.output_path
            .as_deref()
            .expect("normalized UI runner output path"),
    );
    let http = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(http) => http,
        Err(error) => {
            set_state(
                &record,
                RuntimeState::Failed,
                Some(format!("build UI runner HTTP client: {error}")),
                json!({ "outputPath": output }),
            )
            .await;
            return;
        }
    };

    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "starting {} UI runner (attempt {attempt})",
                spec.target.as_str()
            )),
            json!({
                "target": spec.target.as_str(),
                "hostPort": spec.host_port,
                "devicePort": spec.device_port,
                "outputPath": output,
            }),
        )
        .await;

        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.host_port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({
                        "target": spec.target.as_str(),
                        "hostPort": spec.host_port,
                        "devicePort": spec.device_port,
                        "outputPath": output,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };

        let tunnel =
            match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
                Ok(tunnel) => tunnel.map(|(_, tunnel)| tunnel),
                Err(error) => {
                    set_state(
                        &record,
                        RuntimeState::BackingOff,
                        Some(error.to_string()),
                        json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                    )
                    .await;
                    if !wait_retry(&cancel, attempt).await {
                        mark_cancelled(&record).await;
                        return;
                    }
                    continue;
                }
            };
        let start = backend.start_ui_runner_session(&spec, tunnel);
        tokio::pin!(start);
        let session = tokio::select! {
            () = cancel.cancelled() => {
                mark_cancelled(&record).await;
                return;
            }
            result = &mut start => result,
        };
        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                let state = stream_error_state(&error);
                set_state(
                    &record,
                    state.clone(),
                    Some(error.to_string()),
                    json!({
                        "target": spec.target.as_str(),
                        "hostPort": spec.host_port,
                        "devicePort": spec.device_port,
                        "outputPath": output,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };
        let info = session.info().clone();
        let mut file = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                let _ = session.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(CoreError::Io(error).to_string()),
                    json!({ "outputPath": output }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let mut bytes_written = file.metadata().await.map_or(0, |metadata| metadata.len());
        let mut events_written = 0_u64;

        let forward_cancel = cancel.child_token();
        let forward_spec = ForwardSpec {
            udid: spec.udid.clone(),
            bind_host: spec.bind_host.clone(),
            host_port: spec.host_port,
            device_port: spec.device_port,
            connection: rust_ios_protocol::ConnectionPreference::UsbPreferred,
        };
        let forward_backend = backend.clone();
        let forward_token = forward_cancel.clone();
        let mut forward = tokio::spawn(async move {
            run_forward_listener(listener, forward_backend, forward_spec, forward_token).await
        });
        let health_url = ui_runner_health_url(&spec);
        let base_url = health_url
            .strip_suffix(&spec.health_path)
            .unwrap_or(&health_url)
            .to_owned();
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(spec.readiness_timeout_ms);
        let mut readiness = tokio::time::interval(Duration::from_millis(250));
        readiness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_health_error = None;
        let ready = loop {
            let outcome = tokio::select! {
                () = cancel.cancelled() => break Ok(None),
                joined = &mut forward => break Err(ui_runner_forward_error(joined)),
                _ = readiness.tick() => {
                    if let Err(error) = session.ensure_running().await {
                        Err(error)
                    } else if let Err(error) = append_ui_runner_events(
                        &mut *session,
                        &mut file,
                        &mut events_written,
                        &mut bytes_written,
                    )
                    .await
                    {
                        Err(error)
                    } else {
                        match fetch_ui_runner_health(&http, &health_url).await {
                            Ok(health) => Ok(Some(health)),
                            Err(error) if tokio::time::Instant::now() < deadline => {
                                last_health_error = Some(error.to_string());
                                Ok(None)
                            }
                            Err(error) => {
                                let detail = last_health_error
                                    .clone()
                                    .unwrap_or_else(|| error.to_string());
                                Err(CoreError::Timeout(format!(
                                    "{} UI runner did not become ready within {} ms: {detail}",
                                    spec.target.as_str(),
                                    spec.readiness_timeout_ms,
                                )))
                            }
                        }
                    }
                }
            };
            match outcome {
                Ok(Some(health)) => break Ok(Some(health)),
                Ok(None) => {
                    if cancel.is_cancelled() {
                        break Ok(None);
                    }
                }
                Err(error) => break Err(error),
            }
        };

        let run_error = match ready {
            Ok(None) => None,
            Err(error) => Some(error),
            Ok(Some(mut health)) => {
                set_state(
                    &record,
                    RuntimeState::Running,
                    Some(format!(
                        "{} UI runner is ready for {}",
                        spec.target.as_str(),
                        info.udid
                    )),
                    ui_runner_runtime(
                        &spec,
                        &info,
                        &base_url,
                        &output,
                        health.clone(),
                        events_written,
                        bytes_written,
                    ),
                )
                .await;
                let task_id = record.read().await.id;
                info!(
                    %task_id,
                    target = spec.target.as_str(),
                    udid = %info.udid,
                    url = %base_url,
                    "UI runner task running"
                );
                let mut health_tick = tokio::time::interval(Duration::from_secs(1));
                health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                health_tick.tick().await;
                let mut health_failures = 0_u8;
                loop {
                    let outcome = tokio::select! {
                        () = cancel.cancelled() => break None,
                        joined = &mut forward => break Some(ui_runner_forward_error(joined)),
                        _ = health_tick.tick() => {
                            if let Err(error) = session.ensure_running().await {
                                Err(error)
                            } else if let Err(error) = append_ui_runner_events(
                                &mut *session,
                                &mut file,
                                &mut events_written,
                                &mut bytes_written,
                            )
                            .await
                            {
                                Err(error)
                            } else {
                                match fetch_ui_runner_health(&http, &health_url).await {
                                    Ok(next) => {
                                        health = next;
                                        health_failures = 0;
                                        set_state(
                                            &record,
                                            RuntimeState::Running,
                                            Some(format!(
                                                "{} UI runner is ready for {}",
                                                spec.target.as_str(),
                                                info.udid
                                            )),
                                            ui_runner_runtime(
                                                &spec,
                                                &info,
                                                &base_url,
                                                &output,
                                                health.clone(),
                                                events_written,
                                                bytes_written,
                                            ),
                                        )
                                        .await;
                                        Ok(())
                                    }
                                    Err(error) => {
                                        health_failures = health_failures.saturating_add(1);
                                        if health_failures >= 3 {
                                            Err(error)
                                        } else {
                                            Ok(())
                                        }
                                    }
                                }
                            }
                        }
                    };
                    if let Err(error) = outcome {
                        break Some(error);
                    }
                }
            }
        };

        let _ = session.close().await;
        forward_cancel.cancel();
        if !forward.is_finished() {
            let _ = forward.await;
        }
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let error = run_error.unwrap_or_else(|| {
            CoreError::DeviceUnavailable("UI runner stopped unexpectedly".into())
        });
        let state = stream_error_state(&error);
        set_state(
            &record,
            state.clone(),
            Some(error.to_string()),
            json!({
                "target": spec.target.as_str(),
                "url": base_url,
                "hostPort": spec.host_port,
                "devicePort": spec.device_port,
                "outputPath": output,
                "eventsWritten": events_written,
                "bytesWritten": bytes_written,
                "retryInMs": retry_delay(attempt).as_millis(),
            }),
        )
        .await;
        if state == RuntimeState::Failed || !wait_retry(&cancel, attempt).await {
            if cancel.is_cancelled() {
                mark_cancelled(&record).await;
            }
            return;
        }
    }
}

fn ui_runner_health_url(spec: &UiRunnerSpec) -> String {
    let host = match spec.bind_host.as_str() {
        "0.0.0.0" => "127.0.0.1".into(),
        "::" => "[::1]".into(),
        value if value.contains(':') && !value.starts_with('[') => format!("[{value}]"),
        value => value.into(),
    };
    format!("http://{host}:{}{}", spec.host_port, spec.health_path)
}

async fn fetch_ui_runner_health(http: &reqwest::Client, health_url: &str) -> CoreResult<Value> {
    let response = http.get(health_url).send().await.map_err(|error| {
        CoreError::DeviceUnavailable(format!("UI runner health request: {error}"))
    })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| {
        CoreError::DeviceUnavailable(format!("read UI runner health response: {error}"))
    })?;
    if !status.is_success() {
        return Err(CoreError::DeviceUnavailable(format!(
            "UI runner health endpoint returned HTTP {status}"
        )));
    }
    Ok(serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        json!({
            "httpStatus": status.as_u16(),
            "body": String::from_utf8_lossy(&bytes),
        })
    }))
}

async fn append_ui_runner_events(
    session: &mut dyn UiRunnerSession,
    file: &mut tokio::fs::File,
    events_written: &mut u64,
    bytes_written: &mut u64,
) -> CoreResult<()> {
    let mut wrote = false;
    while let Some(event) = session.try_next_event()? {
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| CoreError::Backend(format!("encode UI runner event: {error}")))?;
        line.push(b'\n');
        file.write_all(&line).await?;
        *events_written = events_written.saturating_add(1);
        *bytes_written = bytes_written.saturating_add(line.len() as u64);
        wrote = true;
    }
    if wrote {
        file.flush().await?;
    }
    Ok(())
}

fn ui_runner_forward_error(
    result: Result<Result<(), CoreError>, tokio::task::JoinError>,
) -> CoreError {
    match result {
        Ok(Ok(())) => CoreError::DeviceUnavailable("UI runner forward ended unexpectedly".into()),
        Ok(Err(error)) => error,
        Err(error) => CoreError::Backend(format!("UI runner forward task failed: {error}")),
    }
}

fn ui_runner_runtime(
    spec: &UiRunnerSpec,
    info: &UiRunnerSessionInfo,
    base_url: &str,
    output: &Path,
    health: Value,
    events_written: u64,
    bytes_written: u64,
) -> Value {
    json!({
        "target": spec.target.as_str(),
        "udid": info.udid,
        "url": base_url,
        "bindHost": spec.bind_host,
        "hostPort": spec.host_port,
        "devicePort": spec.device_port,
        "healthPath": spec.health_path,
        "runnerBundleId": spec.runner_bundle_id,
        "targetBundleId": spec.target_bundle_id,
        "xctestConfig": spec.xctest_config,
        "outputPath": output,
        "contentType": "application/x-ndjson",
        "serviceTransport": info.service_transport,
        "health": health,
        "eventsWritten": events_written,
        "bytesWritten": bytes_written,
    })
}

async fn supervise_wda(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: WdaSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output = PathBuf::from(
        spec.output_path
            .as_deref()
            .expect("normalized WDA output path"),
    );
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            set_state(
                &record,
                RuntimeState::Failed,
                Some(CoreError::Io(error).to_string()),
                json!({
                    "outputPath": output,
                    "contentType": "application/x-ndjson",
                }),
            )
            .await;
            return;
        }
    };
    let mut bytes_written = file.metadata().await.map_or(0, |metadata| metadata.len());
    let mut events_written = 0_u64;

    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("starting WDA runner (attempt {attempt})")),
            json!({
                "runnerBundleId": spec.runner_bundle_id,
                "outputPath": output,
                "contentType": "application/x-ndjson",
                "eventsWritten": events_written,
                "bytesWritten": bytes_written,
            }),
        )
        .await;

        let tunnel =
            match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
                Ok(tunnel) => tunnel.map(|(_, tunnel)| tunnel),
                Err(error) => {
                    set_state(
                        &record,
                        RuntimeState::BackingOff,
                        Some(error.to_string()),
                        json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                    )
                    .await;
                    if !wait_retry(&cancel, attempt).await {
                        mark_cancelled(&record).await;
                        return;
                    }
                    continue;
                }
            };
        let start = backend.start_wda_session(&spec, tunnel);
        tokio::pin!(start);
        let session = tokio::select! {
            () = cancel.cancelled() => {
                mark_cancelled(&record).await;
                return;
            }
            result = &mut start => result,
        };
        let mut session = match session {
            Ok(session) => session,
            Err(error) => {
                let retryable = error.retryable()
                    && !matches!(
                        error,
                        CoreError::InvalidArgument(_)
                            | CoreError::NotImplemented(_)
                            | CoreError::UnsupportedPlatform(_)
                            | CoreError::PermissionDenied(_)
                    );
                let state = if matches!(
                    error,
                    CoreError::DeviceNotFound(_)
                        | CoreError::DeviceUnavailable(_)
                        | CoreError::UsbmuxdUnavailable(_)
                ) {
                    RuntimeState::Waiting
                } else if retryable {
                    RuntimeState::BackingOff
                } else {
                    RuntimeState::Failed
                };
                set_state(
                    &record,
                    state,
                    Some(error.to_string()),
                    json!({
                        "outputPath": output,
                        "contentType": "application/x-ndjson",
                        "eventsWritten": events_written,
                        "bytesWritten": bytes_written,
                        "retryInMs": retry_delay(attempt).as_millis(),
                    }),
                )
                .await;
                if !retryable || !wait_retry(&cancel, attempt).await {
                    if cancel.is_cancelled() {
                        mark_cancelled(&record).await;
                    }
                    return;
                }
                continue;
            }
        };

        let info = session.info().clone();
        if let Err(error) = append_wda_events(
            &mut *session,
            &mut file,
            &mut events_written,
            &mut bytes_written,
        )
        .await
        {
            let _ = session.close().await;
            set_state(
                &record,
                RuntimeState::Failed,
                Some(error.to_string()),
                json!({
                    "outputPath": output,
                    "contentType": "application/x-ndjson",
                    "eventsWritten": events_written,
                    "bytesWritten": bytes_written,
                }),
            )
            .await;
            return;
        }
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!("WDA is ready for {}", info.udid)),
            wda_runtime(
                &info,
                info.status.clone(),
                &output,
                events_written,
                bytes_written,
            ),
        )
        .await;
        let task_id = record.read().await.id;
        info!(
            %task_id,
            udid = %info.udid,
            wda_url = %info.wda_url,
            mjpeg_url = %info.mjpeg_url,
            "WDA task running"
        );

        let mut health = tokio::time::interval(Duration::from_secs(1));
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        health.tick().await;
        let error = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = append_wda_events(
                        &mut *session,
                        &mut file,
                        &mut events_written,
                        &mut bytes_written,
                    )
                    .await;
                    let _ = session.close().await;
                    mark_cancelled(&record).await;
                    return;
                }
                _ = health.tick() => {
                    if let Err(error) = append_wda_events(
                        &mut *session,
                        &mut file,
                        &mut events_written,
                        &mut bytes_written,
                    )
                    .await
                    {
                        break error;
                    }
                    match session.status().await {
                        Ok(status) => {
                            set_state(
                                &record,
                                RuntimeState::Running,
                                Some(format!("WDA is ready for {}", info.udid)),
                                wda_runtime(
                                    &info,
                                    status,
                                    &output,
                                    events_written,
                                    bytes_written,
                                ),
                            )
                            .await;
                        }
                        Err(error) => break error,
                    }
                }
            }
        };
        let _ = append_wda_events(
            &mut *session,
            &mut file,
            &mut events_written,
            &mut bytes_written,
        )
        .await;
        let _ = session.close().await;
        let state = if matches!(&error, CoreError::DeviceUnavailable(_)) {
            RuntimeState::Waiting
        } else {
            RuntimeState::BackingOff
        };
        set_state(
            &record,
            state,
            Some(error.to_string()),
            json!({
                "outputPath": output,
                "contentType": "application/x-ndjson",
                "eventsWritten": events_written,
                "bytesWritten": bytes_written,
                "retryInMs": retry_delay(attempt).as_millis(),
            }),
        )
        .await;
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn append_wda_events(
    session: &mut dyn WdaSession,
    file: &mut tokio::fs::File,
    events_written: &mut u64,
    bytes_written: &mut u64,
) -> CoreResult<()> {
    let mut wrote = false;
    while let Some(event) = session.try_next_event()? {
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| CoreError::Backend(format!("encode WDA event: {error}")))?;
        line.push(b'\n');
        file.write_all(&line).await?;
        *events_written = events_written.saturating_add(1);
        *bytes_written = bytes_written.saturating_add(line.len() as u64);
        wrote = true;
    }
    if wrote {
        file.flush().await?;
    }
    Ok(())
}

fn wda_runtime(
    info: &WdaSessionInfo,
    status: Value,
    output: &Path,
    events_written: u64,
    bytes_written: u64,
) -> Value {
    json!({
        "udid": info.udid,
        "wdaUrl": info.wda_url,
        "mjpegUrl": info.mjpeg_url,
        "deviceHttpPort": info.device_http_port,
        "deviceMjpegPort": info.device_mjpeg_port,
        "serviceTransport": info.service_transport,
        "status": status,
        "outputPath": output,
        "contentType": "application/x-ndjson",
        "eventsWritten": events_written,
        "bytesWritten": bytes_written,
    })
}

async fn supervise_app_launch(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: AppLaunchSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output_path = spec
        .output_path
        .as_deref()
        .expect("normalized app process output path")
        .to_owned();
    increment_attempt(&record).await;
    set_state(
        &record,
        RuntimeState::Starting,
        Some(format!("launching {} with stdio capture", spec.bundle_id)),
        json!({
            "bundleId": spec.bundle_id,
            "outputPath": output_path,
            "contentType": "text/plain; charset=utf-8",
        }),
    )
    .await;

    let tunnel = match lookup_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
        Ok((_, tunnel)) => tunnel,
        Err(error) => {
            finish_app_launch(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({
                    "bundleId": spec.bundle_id,
                    "outputPath": output_path,
                    "contentType": "text/plain; charset=utf-8",
                }),
            )
            .await;
            return;
        }
    };
    let start = backend.start_app_process_session(&spec, Some(tunnel));
    tokio::pin!(start);
    let session = tokio::select! {
        () = cancel.cancelled() => {
            finish_app_launch(
                &record,
                RuntimeState::Failed,
                "app launch was cancelled before it started".into(),
                json!({
                    "bundleId": spec.bundle_id,
                    "outputPath": output_path,
                    "contentType": "text/plain; charset=utf-8",
                }),
            ).await;
            return;
        }
        result = &mut start => result,
    };
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            finish_app_launch(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({
                    "bundleId": spec.bundle_id,
                    "outputPath": output_path,
                    "contentType": "text/plain; charset=utf-8",
                }),
            )
            .await;
            return;
        }
    };
    let info = session.info().clone();
    let mut output = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = session.close().await;
            finish_app_launch(
                &record,
                RuntimeState::Failed,
                format!("open app process output: {error}"),
                app_process_runtime(&info, &output_path, 0, 0),
            )
            .await;
            return;
        }
    };
    let mut bytes_written = output.metadata().await.map_or(0, |metadata| metadata.len());
    let mut chunks_written = 0_u64;
    set_state(
        &record,
        RuntimeState::Running,
        Some(format!("{} is running as PID {}", info.bundle_id, info.pid)),
        app_process_runtime(&info, &output_path, bytes_written, chunks_written),
    )
    .await;
    let task_id = record.read().await.id;
    info!(
        %task_id,
        udid = %info.udid,
        bundle_id = %info.bundle_id,
        pid = info.pid,
        "app process task running"
    );

    loop {
        let chunk = tokio::select! {
            () = cancel.cancelled() => {
                let close = session.close().await;
                let message = close.map_or_else(
                    |error| format!("app process cancelled; process stop failed: {error}"),
                    |_| "app process was cancelled before completion".into(),
                );
                finish_app_launch(
                    &record,
                    RuntimeState::Failed,
                    message,
                    app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                ).await;
                return;
            }
            chunk = session.next_chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                if let Err(error) = output.write_all(&chunk).await {
                    let _ = session.close().await;
                    finish_app_launch(
                        &record,
                        RuntimeState::Failed,
                        format!("write app process output: {error}"),
                        app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                    )
                    .await;
                    return;
                }
                if let Err(error) = output.flush().await {
                    let _ = session.close().await;
                    finish_app_launch(
                        &record,
                        RuntimeState::Failed,
                        format!("flush app process output: {error}"),
                        app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                    )
                    .await;
                    return;
                }
                bytes_written = bytes_written.saturating_add(chunk.len() as u64);
                chunks_written = chunks_written.saturating_add(1);
                set_state(
                    &record,
                    RuntimeState::Running,
                    Some(format!("{} is running as PID {}", info.bundle_id, info.pid)),
                    app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                )
                .await;
            }
            Ok(None) => {
                finish_app_launch(
                    &record,
                    RuntimeState::Completed,
                    format!("{} exited and closed stdio", info.bundle_id),
                    app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                )
                .await;
                return;
            }
            Err(error) => {
                let _ = session.close().await;
                finish_app_launch(
                    &record,
                    RuntimeState::Failed,
                    error.to_string(),
                    app_process_runtime(&info, &output_path, bytes_written, chunks_written),
                )
                .await;
                return;
            }
        }
    }
}

fn app_process_runtime(
    info: &AppProcessSessionInfo,
    output_path: &str,
    bytes_written: u64,
    chunks_written: u64,
) -> Value {
    json!({
        "udid": info.udid,
        "bundleId": info.bundle_id,
        "pid": info.pid,
        "serviceTransport": info.service_transport,
        "outputPath": output_path,
        "contentType": "text/plain; charset=utf-8",
        "bytesWritten": bytes_written,
        "chunksWritten": chunks_written,
    })
}

async fn finish_app_launch(
    record: &RwLock<TaskRecord>,
    state: RuntimeState,
    message: String,
    runtime: Value,
) {
    let mut record = record.write().await;
    record.desired_state = DesiredState::Stopped;
    record.runtime_state = state;
    record.updated_at_ms = unix_time_ms();
    record.message = Some(message);
    record.runtime = runtime;
}

async fn supervise_xctest(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: XCTestSpec,
    cancel: CancellationToken,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output_path = spec
        .output_path
        .as_deref()
        .expect("normalized XCTest output path")
        .to_owned();
    // --junit-output（同步 go-ios `aeda162`）：收集事件流，运行结束（含失败/
    // 取消）时把聚合结果写成 JUnit XML，供 CI 消费；未配置时不占用内存。
    let junit_output_path = spec.junit_output_path.clone();
    let mut junit_events = if junit_output_path.is_some() {
        Some(Vec::new())
    } else {
        None
    };
    increment_attempt(&record).await;
    set_state(
        &record,
        RuntimeState::Starting,
        Some(format!("starting {} XCTest target(s)", spec.targets.len())),
        json!({
            "outputPath": output_path,
            "targetCount": spec.targets.len(),
            "contentType": "application/x-ndjson",
        }),
    )
    .await;

    let tunnel = match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
        Ok(tunnel) => tunnel.map(|(_, tunnel)| tunnel),
        Err(error) => {
            finish_xctest(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({ "outputPath": output_path }),
            )
            .await;
            return;
        }
    };
    let start = backend.start_xctest_session(&spec, tunnel);
    tokio::pin!(start);
    let session = tokio::select! {
        () = cancel.cancelled() => {
            finish_xctest(
                &record,
                RuntimeState::Failed,
                "XCTest was cancelled before it started".into(),
                json!({ "outputPath": output_path }),
            ).await;
            return;
        }
        result = &mut start => result,
    };
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            finish_xctest(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({ "outputPath": output_path }),
            )
            .await;
            return;
        }
    };
    let info = session.info().clone();
    set_state(
        &record,
        RuntimeState::Running,
        Some(format!("XCTest is running on {}", info.udid)),
        xctest_runtime(&info, &output_path, None),
    )
    .await;

    let mut output = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = session.close().await;
            finish_xctest(
                &record,
                RuntimeState::Failed,
                format!("open XCTest output: {error}"),
                xctest_runtime(&info, &output_path, None),
            )
            .await;
            return;
        }
    };

    loop {
        let update = tokio::select! {
            () = cancel.cancelled() => {
                let _ = session.close().await;
                finish_xctest(
                    &record,
                    RuntimeState::Failed,
                    "XCTest was cancelled before completion".into(),
                    xctest_runtime(&info, &output_path, None),
                ).await;
                return;
            }
            update = session.next_update() => update,
        };
        match update {
            Ok(XCTestSessionUpdate::Event(event)) => {
                if let Some(events) = junit_events.as_mut() {
                    events.push(event.clone());
                }
                let mut bytes = match serde_json::to_vec(&event) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let _ = session.close().await;
                        finish_xctest(
                            &record,
                            RuntimeState::Failed,
                            format!("serialize XCTest event: {error}"),
                            xctest_runtime(&info, &output_path, None),
                        )
                        .await;
                        return;
                    }
                };
                bytes.push(b'\n');
                let write_result = async {
                    output.write_all(&bytes).await?;
                    output.flush().await
                }
                .await;
                if let Err(error) = write_result {
                    let _ = session.close().await;
                    finish_xctest(
                        &record,
                        RuntimeState::Failed,
                        format!("write XCTest output: {error}"),
                        xctest_runtime(&info, &output_path, None),
                    )
                    .await;
                    return;
                }
            }
            Ok(XCTestSessionUpdate::Finished(summary)) => {
                let outcome = if summary.failed > 0 || summary.stalled > 0 {
                    "failed"
                } else {
                    "passed"
                };
                let mut runtime = xctest_runtime(&info, &output_path, Some((&summary, outcome)));
                write_xctest_junit(&junit_output_path, junit_events.as_deref(), &mut runtime).await;
                finish_xctest(
                    &record,
                    RuntimeState::Completed,
                    format!(
                        "XCTest completed: {} passed, {} failed, {} skipped",
                        summary.passed, summary.failed, summary.skipped
                    ),
                    runtime,
                )
                .await;
                return;
            }
            Err(error) => {
                let _ = session.close().await;
                let mut runtime = xctest_runtime(&info, &output_path, None);
                write_xctest_junit(&junit_output_path, junit_events.as_deref(), &mut runtime).await;
                finish_xctest(&record, RuntimeState::Failed, error.to_string(), runtime).await;
                return;
            }
        }
    }
}

fn xctest_runtime(
    info: &XCTestSessionInfo,
    output_path: &str,
    completed: Option<(&XCTestRunSummary, &str)>,
) -> Value {
    let mut runtime = json!({
        "udid": info.udid,
        "targetCount": info.target_count,
        "serviceTransport": info.service_transport,
        "outputPath": output_path,
        "contentType": "application/x-ndjson",
    });
    if let Some((summary, outcome)) = completed {
        runtime["outcome"] = Value::String(outcome.into());
        runtime["summary"] = serde_json::to_value(summary).unwrap_or(Value::Null);
    }
    runtime
}

/// 把聚合的 JUnit XML 写到 `--junit-output` 路径（server 代写 client 绝对路径，
/// 与 screenshot 的约定一致）；写盘失败记录进 runtime，不吞掉运行结果。
async fn write_xctest_junit(
    junit_output_path: &Option<String>,
    events: Option<&[rust_ios_protocol::XCTestEvent]>,
    runtime: &mut Value,
) {
    let (Some(path), Some(events)) = (junit_output_path.as_deref(), events) else {
        return;
    };
    let xml = rust_ios_protocol::junit::junit_xml(events);
    match tokio::fs::write(path, xml.as_bytes()).await {
        Ok(()) => {
            runtime["junitOutputPath"] = Value::String(path.to_owned());
        }
        Err(error) => {
            runtime["junitOutputError"] = Value::String(error.to_string());
        }
    }
}

async fn finish_xctest(
    record: &RwLock<TaskRecord>,
    state: RuntimeState,
    message: String,
    runtime: Value,
) {
    let mut record = record.write().await;
    record.desired_state = DesiredState::Stopped;
    record.runtime_state = state;
    record.updated_at_ms = unix_time_ms();
    record.message = Some(message);
    record.runtime = runtime;
}

async fn supervise_webinspector_cdp(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: WebInspectorCdpSpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "starting WebInspector CDP endpoint (attempt {attempt})"
            )),
            json!({ "bindHost": spec.bind_host, "port": spec.port }),
        )
        .await;
        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let tunnel =
            match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
                Ok(tunnel) => tunnel,
                Err(error) => {
                    set_state(
                        &record,
                        RuntimeState::Waiting,
                        Some(error.to_string()),
                        json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                    )
                    .await;
                    if !wait_retry(&cancel, attempt).await {
                        mark_cancelled(&record).await;
                        return;
                    }
                    continue;
                }
            };
        if let Some((_, tunnel)) = tunnel.as_ref()
            && let Err(error) = tunnel
                .ensure_rsd_service(RsdServiceKind::WebInspector)
                .await
        {
            set_state(
                &record,
                RuntimeState::Waiting,
                Some(error.to_string()),
                json!({ "retryInMs": retry_delay(attempt).as_millis() }),
            )
            .await;
            if !wait_retry(&cancel, attempt).await {
                mark_cancelled(&record).await;
                return;
            }
            continue;
        }
        let session = match backend
            .start_webinspector_cdp_session(
                &spec,
                tunnel.as_ref().map(|(_, tunnel)| tunnel.clone()),
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                let state = if matches!(
                    error,
                    CoreError::DeviceNotFound(_)
                        | CoreError::DeviceUnavailable(_)
                        | CoreError::UsbmuxdUnavailable(_)
                ) {
                    RuntimeState::Waiting
                } else {
                    RuntimeState::BackingOff
                };
                set_state(
                    &record,
                    state,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let info = session.info().clone();
        let endpoint = match listener.local_addr() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let mut session = session;
                let _ = session.close().await;
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "WebInspector CDP endpoint is running for {}",
                info.udid
            )),
            json!({
                "udid": info.udid,
                "endpoint": endpoint,
                "host": endpoint.ip().to_string(),
                "port": endpoint.port(),
                "jsonUrl": format!("http://{endpoint}/json"),
                "pageCount": info.pages.len(),
                "serviceTransport": info.service_transport,
            }),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, %endpoint, "WebInspector CDP task running");
        let result = crate::webinspector_cdp::serve(listener, session, cancel.clone()).await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let error = result.err().unwrap_or_else(|| {
            CoreError::Backend("WebInspector CDP endpoint stopped unexpectedly".into())
        });
        let state = if matches!(error, CoreError::DeviceUnavailable(_)) {
            RuntimeState::Waiting
        } else {
            RuntimeState::BackingOff
        };
        set_state(
            &record,
            state,
            Some(error.to_string()),
            json!({ "retryInMs": retry_delay(attempt).as_millis() }),
        )
        .await;
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn supervise_webinspector_js_shell(
    record: Arc<RwLock<TaskRecord>>,
    backend: Arc<dyn DeviceBackend>,
    spec: WebInspectorJsShellSpec,
    cancel: CancellationToken,
    mut commands: mpsc::Receiver<WebInspectorJsShellCommand>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    let output_path = spec
        .output_path
        .as_deref()
        .expect("normalized WebInspector JS shell output path")
        .to_owned();
    increment_attempt(&record).await;
    set_state(
        &record,
        RuntimeState::Starting,
        Some("starting WebInspector JS shell".into()),
        json!({
            "outputPath": output_path,
            "contentType": "application/x-ndjson",
        }),
    )
    .await;
    let tunnel = match lookup_optional_active_tunnel(&active_tunnels, spec.udid.as_deref()).await {
        Ok(tunnel) => tunnel,
        Err(error) => {
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({ "outputPath": output_path }),
            )
            .await;
            return;
        }
    };
    if let Some((_, tunnel)) = tunnel.as_ref()
        && let Err(error) = tunnel
            .ensure_rsd_service(RsdServiceKind::WebInspector)
            .await
    {
        finish_webinspector_js_shell(
            &record,
            RuntimeState::Failed,
            error.to_string(),
            json!({ "outputPath": output_path }),
        )
        .await;
        return;
    }
    let start = timeout(
        Duration::from_millis(spec.command_timeout_ms),
        backend.start_webinspector_js_shell_session(
            &spec,
            tunnel.as_ref().map(|(_, tunnel)| tunnel.clone()),
        ),
    );
    tokio::pin!(start);
    let session = tokio::select! {
        () = cancel.cancelled() => {
            mark_cancelled(&record).await;
            return;
        }
        result = &mut start => match result {
            Ok(result) => result,
            Err(_) => Err(CoreError::Timeout(format!(
                "starting WebInspector JS shell after {:.3} seconds",
                Duration::from_millis(spec.command_timeout_ms).as_secs_f64()
            ))),
        },
    };
    let mut session = match session {
        Ok(session) => session,
        Err(error) => {
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                error.to_string(),
                json!({ "outputPath": output_path }),
            )
            .await;
            return;
        }
    };
    let info = session.info().clone();
    let mut output = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let _ = session.close().await;
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                format!("open WebInspector JS shell output: {error}"),
                json!({ "outputPath": output_path }),
            )
            .await;
            return;
        }
    };
    let mut sequence = 0_u64;
    let mut bytes_written = 0_u64;
    set_state(
        &record,
        RuntimeState::Running,
        Some(format!(
            "WebInspector JS shell is attached to {} on {}",
            info.page.key, info.udid
        )),
        webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
    )
    .await;

    loop {
        let command = tokio::select! {
            () = cancel.cancelled() => {
                let _ = session.close().await;
                mark_cancelled(&record).await;
                return;
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            let _ = session.close().await;
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                "WebInspector JS shell input channel closed unexpectedly".into(),
                webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
            )
            .await;
            return;
        };
        let result = tokio::select! {
            () = cancel.cancelled() => {
                let _ = command.response.send(Err(CoreError::DeviceUnavailable(
                    "WebInspector JS shell was stopped during evaluation".into(),
                )));
                let _ = session.close().await;
                mark_cancelled(&record).await;
                return;
            }
            result = session.evaluate(&command.expression) => result,
        };
        sequence = sequence.saturating_add(1);
        let entry = match &result {
            Ok(value) => json!({
                "sequence": sequence,
                "expression": &command.expression,
                "result": value,
            }),
            Err(error) => json!({
                "sequence": sequence,
                "expression": &command.expression,
                "error": error.to_string(),
            }),
        };
        let mut line = match serde_json::to_vec(&entry) {
            Ok(line) => line,
            Err(error) => {
                let message = format!("serialize WebInspector JS shell output: {error}");
                let _ = command
                    .response
                    .send(Err(CoreError::Backend(message.clone())));
                let _ = session.close().await;
                finish_webinspector_js_shell(
                    &record,
                    RuntimeState::Failed,
                    message,
                    webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
                )
                .await;
                return;
            }
        };
        line.push(b'\n');
        if let Err(error) = output.write_all(&line).await {
            let message = format!("write WebInspector JS shell output: {error}");
            let _ = command.response.send(Err(CoreError::Io(io::Error::new(
                error.kind(),
                message.clone(),
            ))));
            let _ = session.close().await;
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                message,
                webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
            )
            .await;
            return;
        }
        if let Err(error) = output.flush().await {
            let message = format!("flush WebInspector JS shell output: {error}");
            let _ = command.response.send(Err(CoreError::Io(io::Error::new(
                error.kind(),
                message.clone(),
            ))));
            let _ = session.close().await;
            finish_webinspector_js_shell(
                &record,
                RuntimeState::Failed,
                message,
                webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
            )
            .await;
            return;
        }
        bytes_written = bytes_written.saturating_add(line.len() as u64);
        let _ = command.response.send(result);
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!(
                "WebInspector JS shell evaluated {sequence} expression(s)"
            )),
            webinspector_js_shell_runtime(&info, &output_path, sequence, bytes_written),
        )
        .await;
    }
}

fn webinspector_js_shell_runtime(
    info: &rust_ios_core::WebInspectorJsShellSessionInfo,
    output_path: &str,
    evaluations: u64,
    bytes_written: u64,
) -> Value {
    json!({
        "udid": info.udid,
        "page": info.page,
        "serviceTransport": info.service_transport,
        "outputPath": output_path,
        "contentType": "application/x-ndjson",
        "evaluations": evaluations,
        "bytesWritten": bytes_written,
    })
}

async fn finish_webinspector_js_shell(
    record: &RwLock<TaskRecord>,
    state: RuntimeState,
    message: String,
    runtime: Value,
) {
    let mut record = record.write().await;
    record.desired_state = DesiredState::Stopped;
    record.runtime_state = state;
    record.updated_at_ms = unix_time_ms();
    record.message = Some(message);
    record.runtime = runtime;
}

async fn supervise_dproxy(
    record: Arc<RwLock<TaskRecord>>,
    spec: DproxySpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!(
                "starting usbmuxd capture proxy (attempt {attempt})"
            )),
            json!({ "bindHost": spec.bind_host, "port": spec.port }),
        )
        .await;
        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let endpoint = match listener.local_addr() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    Value::Null,
                )
                .await;
                continue;
            }
        };
        let output_path = PathBuf::from(
            spec.output_path
                .as_deref()
                .expect("normalized dproxy output path"),
        );
        let mut capture_root = output_path.as_os_str().to_os_string();
        capture_root.push(".d");
        let capture_root = PathBuf::from(capture_root);
        if let Err(error) = tokio::fs::create_dir_all(&capture_root).await {
            set_state(
                &record,
                RuntimeState::BackingOff,
                Some(format!("create dproxy capture directory: {error}")),
                json!({ "retryInMs": retry_delay(attempt).as_millis() }),
            )
            .await;
            if !wait_retry(&cancel, attempt).await {
                mark_cancelled(&record).await;
                return;
            }
            continue;
        }
        let output = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_path)
            .await
        {
            Ok(output) => Arc::new(Mutex::new(output)),
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(format!("open dproxy event output: {error}")),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let counters = Arc::new(DproxyRuntimeCounters::default());
        let generation = record.read().await.generation;
        set_state(
            &record,
            RuntimeState::Running,
            Some("usbmuxd capture proxy is running".into()),
            dproxy_runtime(&spec, endpoint, &output_path, &capture_root, &counters),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, %endpoint, upstream = %spec.upstream_address, "dproxy task running");
        let result = run_dproxy_endpoint(
            record.clone(),
            listener,
            spec.clone(),
            endpoint,
            output_path.clone(),
            capture_root.clone(),
            generation,
            counters,
            output,
            cancel.clone(),
        )
        .await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        set_state(
            &record,
            RuntimeState::BackingOff,
            result.err().map(|error| error.to_string()),
            json!({ "retryInMs": retry_delay(attempt).as_millis() }),
        )
        .await;
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_dproxy_endpoint(
    record: Arc<RwLock<TaskRecord>>,
    listener: TcpListener,
    spec: DproxySpec,
    endpoint: std::net::SocketAddr,
    output_path: PathBuf,
    capture_root: PathBuf,
    generation: u64,
    counters: Arc<DproxyRuntimeCounters>,
    output: Arc<Mutex<tokio::fs::File>>,
    cancel: CancellationToken,
) -> CoreResult<()> {
    let upstream_address = spec
        .upstream_address
        .parse::<std::net::SocketAddr>()
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let mut connections = JoinSet::new();
    let mut sequence = 0_u64;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                sequence = sequence.saturating_add(1);
                let connection_id = format!("generation-{generation}-connection-{sequence}");
                let connection_directory = capture_root
                    .join(format!("generation-{generation}"))
                    .join(format!("connection-{sequence}"));
                counters.accepted.fetch_add(1, Ordering::SeqCst);
                counters.active.fetch_add(1, Ordering::SeqCst);
                append_dproxy_event(
                    &output,
                    json!({
                        "timestampMs": unix_time_ms(),
                        "event": "connection_open",
                        "connectionId": connection_id,
                        "peer": peer,
                        "captureDirectory": connection_directory,
                    }),
                )
                .await?;
                update_dproxy_runtime(
                    &record,
                    &spec,
                    endpoint,
                    &output_path,
                    &capture_root,
                    &counters,
                )
                .await;
                let record = record.clone();
                let spec = spec.clone();
                let counters = counters.clone();
                let output = output.clone();
                let capture_root = capture_root.clone();
                let output_path = output_path.clone();
                let cancel = cancel.clone();
                connections.spawn(async move {
                    let result = run_dproxy_connection(
                        Box::new(client),
                        upstream_address,
                        DproxyCaptureSpec {
                            connection_id: connection_id.clone(),
                            directory: connection_directory.clone(),
                            binary_only: spec.binary_only,
                        },
                        cancel,
                    )
                    .await;
                    counters.active.fetch_sub(1, Ordering::SeqCst);
                    let event = match &result {
                        Ok(stats) => {
                            counters.completed.fetch_add(1, Ordering::SeqCst);
                            counters.host_to_usbmuxd_bytes.fetch_add(
                                stats.host_to_usbmuxd_bytes,
                                Ordering::SeqCst,
                            );
                            counters.usbmuxd_to_host_bytes.fetch_add(
                                stats.usbmuxd_to_host_bytes,
                                Ordering::SeqCst,
                            );
                            json!({
                                "timestampMs": unix_time_ms(),
                                "event": "connection_closed",
                                "connectionId": connection_id,
                                "stats": stats,
                            })
                        }
                        Err(error) => {
                            counters.failed.fetch_add(1, Ordering::SeqCst);
                            json!({
                                "timestampMs": unix_time_ms(),
                                "event": "connection_failed",
                                "connectionId": connection_id,
                                "captureDirectory": connection_directory,
                                "error": error.to_string(),
                            })
                        }
                    };
                    if let Err(error) = append_dproxy_event(&output, event).await {
                        warn!(%error, "write dproxy connection event failed");
                    }
                    update_dproxy_runtime(
                        &record,
                        &spec,
                        endpoint,
                        &output_path,
                        &capture_root,
                        &counters,
                    )
                    .await;
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    warn!(%error, "dproxy connection task panicked");
                }
            }
        }
    }
    if timeout(Duration::from_secs(3), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

async fn append_dproxy_event(output: &Mutex<tokio::fs::File>, event: Value) -> CoreResult<()> {
    let mut line = serde_json::to_vec(&event)
        .map_err(|error| CoreError::Backend(format!("serialize dproxy event: {error}")))?;
    line.push(b'\n');
    let mut output = output.lock().await;
    output.write_all(&line).await?;
    output.flush().await?;
    Ok(())
}

async fn update_dproxy_runtime(
    record: &RwLock<TaskRecord>,
    spec: &DproxySpec,
    endpoint: std::net::SocketAddr,
    output_path: &Path,
    capture_root: &Path,
    counters: &DproxyRuntimeCounters,
) {
    let mut record = record.write().await;
    if record.runtime_state == RuntimeState::Running {
        record.runtime = dproxy_runtime(spec, endpoint, output_path, capture_root, counters);
        record.updated_at_ms = unix_time_ms();
        record.message = Some(format!(
            "usbmuxd capture proxy accepted {} connection(s)",
            counters.accepted.load(Ordering::SeqCst)
        ));
    }
}

fn dproxy_runtime(
    spec: &DproxySpec,
    endpoint: std::net::SocketAddr,
    output_path: &Path,
    capture_root: &Path,
    counters: &DproxyRuntimeCounters,
) -> Value {
    json!({
        "endpoint": endpoint,
        "host": endpoint.ip().to_string(),
        "port": endpoint.port(),
        "upstream": spec.upstream_address,
        "mode": spec.mode,
        "binaryOnly": spec.binary_only,
        "outputPath": output_path,
        "captureDirectory": capture_root,
        "contentType": "application/x-ndjson",
        "connectionsAccepted": counters.accepted.load(Ordering::SeqCst),
        "connectionsActive": counters.active.load(Ordering::SeqCst),
        "connectionsCompleted": counters.completed.load(Ordering::SeqCst),
        "connectionsFailed": counters.failed.load(Ordering::SeqCst),
        "hostToUsbmuxdBytes": counters.host_to_usbmuxd_bytes.load(Ordering::SeqCst),
        "usbmuxdToHostBytes": counters.usbmuxd_to_host_bytes.load(Ordering::SeqCst),
    })
}

async fn supervise_debug_proxy(
    record: Arc<RwLock<TaskRecord>>,
    spec: DebugProxySpec,
    cancel: CancellationToken,
    mut prebound: Option<TcpListener>,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
) {
    loop {
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let attempt = increment_attempt(&record).await;
        set_state(
            &record,
            RuntimeState::Starting,
            Some(format!("starting debug proxy (attempt {attempt})")),
            json!({ "bindHost": spec.bind_host, "port": spec.port }),
        )
        .await;
        let listener = match prebound.take() {
            Some(listener) => Ok(listener),
            None => TcpListener::bind((spec.bind_host.as_str(), spec.port))
                .await
                .map_err(CoreError::Io),
        };
        let listener = match listener {
            Ok(listener) => listener,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        let (udid, tunnel) = match lookup_active_tunnel(&active_tunnels, spec.udid.as_deref()).await
        {
            Ok(active) => active,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::Waiting,
                    Some(error.to_string()),
                    json!({ "retryInMs": retry_delay(attempt).as_millis() }),
                )
                .await;
                if !wait_retry(&cancel, attempt).await {
                    mark_cancelled(&record).await;
                    return;
                }
                continue;
            }
        };
        if let Err(error) = tunnel.ensure_rsd_service(RsdServiceKind::DebugProxy).await {
            set_state(
                &record,
                RuntimeState::Waiting,
                Some(error.to_string()),
                json!({ "retryInMs": retry_delay(attempt).as_millis() }),
            )
            .await;
            if !wait_retry(&cancel, attempt).await {
                mark_cancelled(&record).await;
                return;
            }
            continue;
        }
        let endpoint = match listener.local_addr() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                set_state(
                    &record,
                    RuntimeState::BackingOff,
                    Some(error.to_string()),
                    Value::Null,
                )
                .await;
                continue;
            }
        };
        set_state(
            &record,
            RuntimeState::Running,
            Some(format!("debug proxy is running for {udid}")),
            json!({
                "udid": udid,
                "endpoint": endpoint,
                "host": endpoint.ip().to_string(),
                "port": endpoint.port(),
                "protocol": "gdb-remote",
            }),
        )
        .await;
        let task_id = record.read().await.id;
        info!(%task_id, %udid, %endpoint, "debug proxy task running");
        let result =
            run_debug_proxy_endpoint(listener, &udid, active_tunnels.clone(), cancel.clone()).await;
        if cancel.is_cancelled() {
            mark_cancelled(&record).await;
            return;
        }
        let state = if matches!(result, Err(CoreError::DeviceUnavailable(_))) {
            RuntimeState::Waiting
        } else {
            RuntimeState::BackingOff
        };
        set_state(
            &record,
            state,
            result.err().map(|error| error.to_string()),
            json!({ "retryInMs": retry_delay(attempt).as_millis() }),
        )
        .await;
        if !wait_retry(&cancel, attempt).await {
            mark_cancelled(&record).await;
            return;
        }
    }
}

async fn run_debug_proxy_endpoint(
    listener: TcpListener,
    udid: &str,
    active_tunnels: Arc<RwLock<HashMap<String, ActiveTunnel>>>,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
    let mut connections = JoinSet::new();
    let mut health = tokio::time::interval(Duration::from_millis(250));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    health.tick().await;
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                return Ok(());
            }
            _ = health.tick() => {
                lookup_active_tunnel(&active_tunnels, Some(udid)).await?;
            }
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                let (_, tunnel) = lookup_active_tunnel(&active_tunnels, Some(udid)).await?;
                let connection_cancel = cancel.child_token();
                connections.spawn(async move {
                    let device = match tunnel
                        .connect_rsd_service(RsdServiceKind::DebugProxy)
                        .await
                    {
                        Ok(device) => device,
                        Err(error) => {
                            debug!(%peer, %error, "unable to open RSD debug proxy service");
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        bridge_streams(Box::new(client), device, connection_cancel).await
                    {
                        debug!(%peer, %error, "debug proxy connection ended with error");
                        return Err(error);
                    }
                    Ok(())
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => {
                        return Err(CoreError::Backend(format!(
                            "debug proxy connection task panicked: {error}"
                        )));
                    }
                }
            }
        }
    }
}

/// 数据面探针连续失败多少次才中断隧道并触发重建。同步 go-ios fork `3172596` 的
/// 防抖语义(该提交针对 go-ios agent 的 `UpdateTunnels`;rust 隧道与设备枚举解耦,
/// 但探针原实现同样"单次失败即 abort 全部连接并重建",重建瞬间上层连接全断,
/// WDA/XCTest 会话同样受影响)。真正的数据面死亡仍在约 2 秒 × 阈值内被识别。
const PROBE_FAILURES_BEFORE_REBUILD: u32 = 3;

/// 单次数据面探针失败的处理决策(防抖)。
#[derive(Debug, PartialEq, Eq)]
enum ProbeFailureVerdict {
    /// 未达阈值:告警并保留隧道与现有连接,计数等待下一次探针。
    KeepTunnel,
    /// 达到阈值:中断探针循环,交给 supervisor 退避重建。
    Rebuild,
}

fn probe_failure_verdict(failures: u32) -> ProbeFailureVerdict {
    if failures < PROBE_FAILURES_BEFORE_REBUILD {
        ProbeFailureVerdict::KeepTunnel
    } else {
        ProbeFailureVerdict::Rebuild
    }
}

async fn run_tunnel_endpoint(
    listener: TcpListener,
    tunnel: Arc<dyn UserSpaceTunnel>,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
    let mut connections = JoinSet::new();
    let mut health = tokio::time::interval(Duration::from_secs(2));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    health.tick().await;
    let mut probe_failures: u32 = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                return Ok(());
            }
            _ = health.tick() => {
                let rsd_port = tunnel.info().rsd_port;
                let probe =
                    tokio::time::timeout(Duration::from_secs(5), tunnel.connect(rsd_port)).await;
                let failure = match probe {
                    Ok(Ok(stream)) => {
                        if probe_failures > 0 {
                            debug!(%rsd_port, "userspace tunnel RSD data-plane probe recovered");
                        }
                        probe_failures = 0;
                        drop(stream);
                        None
                    }
                    Ok(Err(error)) => Some(format!("failed: {error}")),
                    Err(_) => Some("timed out after 5 seconds".to_string()),
                };
                if let Some(detail) = failure {
                    probe_failures += 1;
                    match probe_failure_verdict(probe_failures) {
                        ProbeFailureVerdict::KeepTunnel => {
                            warn!(
                                %rsd_port,
                                failures = probe_failures,
                                threshold = PROBE_FAILURES_BEFORE_REBUILD,
                                "userspace tunnel RSD data-plane probe {detail}; keeping tunnel and existing connections"
                            );
                        }
                        ProbeFailureVerdict::Rebuild => {
                            connections.abort_all();
                            while connections.join_next().await.is_some() {}
                            return Err(CoreError::DeviceUnavailable(format!(
                                "userspace tunnel RSD data-plane health check {detail} on port {rsd_port} ({PROBE_FAILURES_BEFORE_REBUILD} consecutive probe failures)"
                            )));
                        }
                    }
                }
            }
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                let tunnel = tunnel.clone();
                let connection_cancel = cancel.child_token();
                connections.spawn(async move {
                    if let Err(error) = handle_userspace_connection(client, tunnel, connection_cancel).await {
                        debug!(%peer, %error, "userspace tunnel connection failed");
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    debug!(%error, "userspace tunnel connection task panicked");
                }
            }
        }
    }
}

async fn handle_userspace_connection(
    mut client: TcpStream,
    tunnel: Arc<dyn UserSpaceTunnel>,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
    let mut address = [0_u8; 16];
    client.read_exact(&mut address).await?;
    let target_address = Ipv6Addr::from(address);
    let mut port = [0_u8; 4];
    client.read_exact(&mut port).await?;
    let port = u32::from_le_bytes(port);
    if port == 0 || port > u16::MAX.into() {
        return Err(CoreError::InvalidArgument(format!(
            "invalid userspace tunnel target port {port}"
        )));
    }
    debug!(%target_address, port, "opening userspace tunnel connection");
    let device = tunnel.connect(port as u16).await?;
    bridge_streams(Box::new(client), device, cancel).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::{SinkExt, StreamExt};
    use reqwest::StatusCode;
    use rust_ios_core::MockDeviceBackend;
    use rust_ios_protocol::{OsTraceLevel, OsTraceOptions};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};

    use super::*;

    async fn manager(backend: MockDeviceBackend, path: &std::path::Path) -> Arc<TaskManager> {
        TaskManager::load(Arc::new(backend), StateStore::new(path))
            .await
            .unwrap()
    }

    async fn wait_state(manager: &TaskManager, id: Uuid, state: RuntimeState) -> TaskRecord {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let record = manager.show(id).await.unwrap();
            if record.runtime_state == state {
                return record;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "last record: {record:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn forward_spec() -> ForwardSpec {
        ForwardSpec {
            udid: Some("mock-phone".into()),
            bind_host: "127.0.0.1".into(),
            host_port: 0,
            device_port: 8100,
            connection: rust_ios_protocol::ConnectionPreference::UsbPreferred,
        }
    }

    fn tunnel_spec() -> TunnelSpec {
        TunnelSpec {
            udid: Some("mock-phone".into()),
            mode: TunnelMode::Userspace,
            transport: TunnelTransport::UsbCoreDevice,
            pair_record_path: ".".into(),
            userspace_bind_host: "127.0.0.1".into(),
            userspace_port: 0,
            kernel_interface_name: None,
            wintun_path: None,
        }
    }

    fn kernel_tunnel_spec() -> TunnelSpec {
        TunnelSpec {
            mode: TunnelMode::Kernel,
            kernel_interface_name: Some("rust-ios-mock-phone".into()),
            wintun_path: Some("C:\\Windows\\System32\\wintun.dll".into()),
            ..tunnel_spec()
        }
    }

    fn dproxy_spec(upstream: std::net::SocketAddr, output: &Path) -> DproxySpec {
        DproxySpec {
            bind_host: "127.0.0.1".into(),
            port: 0,
            upstream_address: upstream.to_string(),
            binary_only: false,
            mode: DproxyMode::Usbmuxd,
            interface: None,
            output_path: Some(output.to_string_lossy().into_owned()),
        }
    }

    fn usbmuxd_list_devices_packet() -> Vec<u8> {
        let payload = br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>MessageType</key><string>ListDevices</string></dict></plist>"#;
        let mut packet = Vec::with_capacity(16 + payload.len());
        packet.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
        packet.extend_from_slice(&1_u32.to_le_bytes());
        packet.extend_from_slice(&8_u32.to_le_bytes());
        packet.extend_from_slice(&41_u32.to_le_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    async fn fake_usbmuxd_upstream() -> (
        std::net::SocketAddr,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = task_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 64 * 1024];
                    while let Ok(length) = stream.read(&mut buffer).await {
                        if length == 0 || stream.write_all(&buffer[..length]).await.is_err() {
                            break;
                        }
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });
        (address, cancel, task)
    }

    fn stream_spec(kind: TaskKind, format: StreamFormat) -> StreamTaskSpec {
        StreamTaskSpec {
            kind,
            udid: Some("mock-phone".into()),
            output_path: None,
            format,
            pid: None,
            process: None,
            os_trace: None,
            watch_kill_targets: None,
        }
    }

    fn accessibility_inspector_spec() -> AccessibilityInspectorSpec {
        AccessibilityInspectorSpec {
            udid: Some("mock-phone".into()),
            output_path: None,
            font_size: None,
        }
    }

    fn location_simulation_spec(source: &Path) -> LocationSimulationSpec {
        LocationSimulationSpec {
            udid: Some("mock-phone".into()),
            source_path: source.to_string_lossy().into_owned(),
            points: vec![
                LocationSimulationPoint {
                    latitude: 31.2304,
                    longitude: 121.4737,
                    delay_before_ms: 0,
                    source_time: Some("2026-07-16T00:00:00Z".into()),
                },
                LocationSimulationPoint {
                    latitude: 31.2305,
                    longitude: 121.4738,
                    delay_before_ms: 20,
                    source_time: Some("2026-07-16T00:00:00.020Z".into()),
                },
            ],
            output_path: None,
        }
    }

    fn wda_spec() -> WdaSpec {
        WdaSpec {
            udid: Some("mock-phone".into()),
            runner_bundle_id: "com.example.WebDriverAgentRunner.xctrunner".into(),
            target_bundle_id: None,
            xctest_config: Some("WebDriverAgentRunner.xctest".into()),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            output_path: None,
            readiness_timeout_ms: 30_000,
        }
    }

    fn ui_runner_spec(target: UiRunnerTarget) -> UiRunnerSpec {
        let (bundle_id, xctest_config, device_port, health_path) = match target {
            UiRunnerTarget::Wda => (
                "com.example.WebDriverAgentRunner.xctrunner",
                "WebDriverAgentRunner.xctest",
                8100,
                "/status",
            ),
            UiRunnerTarget::DeviceKit => (
                "com.example.devicekit.runner",
                "devicekit-iosUITests.xctest",
                12004,
                "/health",
            ),
        };
        UiRunnerSpec {
            target,
            udid: Some("mock-phone".into()),
            runner_bundle_id: bundle_id.into(),
            target_bundle_id: bundle_id.into(),
            xctest_config: xctest_config.into(),
            bind_host: "127.0.0.1".into(),
            host_port: 0,
            device_port,
            health_path: health_path.into(),
            readiness_timeout_ms: 3_000,
            output_path: None,
        }
    }

    fn webinspector_cdp_spec() -> WebInspectorCdpSpec {
        WebInspectorCdpSpec {
            udid: Some("mock-phone".into()),
            bind_host: "127.0.0.1".into(),
            port: 0,
            startup_timeout_ms: 15_000,
        }
    }

    fn webinspector_js_shell_spec() -> WebInspectorJsShellSpec {
        WebInspectorJsShellSpec {
            udid: Some("mock-phone".into()),
            url: Some("https://example.com/shell".into()),
            bundle_id: Some("com.apple.mobilesafari".into()),
            open_safari: true,
            command_timeout_ms: 15_000,
            output_path: None,
        }
    }

    fn xctest_spec() -> XCTestSpec {
        XCTestSpec {
            udid: Some("mock-phone".into()),
            targets: vec![rust_ios_protocol::XCTestTargetSpec {
                name: "DemoUITests".into(),
                runner_bundle_id: "com.example.DemoUITests.xctrunner".into(),
                target_bundle_id: Some("com.example.Demo".into()),
                target_app_name: None,
                xctest_config: Some("DemoUITests.xctest".into()),
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                tests_to_run: Vec::new(),
                tests_to_skip: Vec::new(),
                is_ui_test: true,
            }],
            output_path: None,
            junit_output_path: None,
            source_xctestrun_path: None,
            timeout_ms: None,
        }
    }

    fn app_launch_spec() -> AppLaunchSpec {
        AppLaunchSpec {
            udid: Some("mock-phone".into()),
            bundle_id: "com.example.Demo".into(),
            arguments: vec!["--example".into()],
            environment: BTreeMap::from([("FEATURE".into(), "true".into())]),
            kill_existing: true,
            output_path: None,
        }
    }

    async fn wait_output(manager: &TaskManager, id: Uuid, minimum_bytes: u64) -> TaskOutput {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let output = manager.read_output(id, 0, 1024 * 1024).await.unwrap();
            if output.total_bytes >= minimum_bytes {
                return output;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_output_contains(manager: &TaskManager, id: Uuid, needle: &str) -> TaskOutput {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let output = manager.read_output(id, 0, 1024 * 1024).await.unwrap();
            let bytes = BASE64.decode(&output.bytes_base64).unwrap();
            if String::from_utf8_lossy(&bytes).contains(needle) {
                return output;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn ui_stream_source(
        expected_target: &'static str,
    ) -> (String, Arc<AtomicUsize>, CancellationToken, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_connections = connections.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = task_cancel.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let (mut socket, _) = accepted.unwrap();
                let connection_cancel = task_cancel.clone();
                let connection_count = task_connections.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    loop {
                        let count = socket.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    assert!(
                        String::from_utf8_lossy(&request)
                            .starts_with(&format!("GET {expected_target} HTTP/1.1"))
                    );
                    connection_count.fetch_add(1, Ordering::SeqCst);
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    let mut ticker = tokio::time::interval(Duration::from_millis(10));
                    loop {
                        tokio::select! {
                            () = connection_cancel.cancelled() => return,
                            _ = ticker.tick() => {
                                if socket.write_all(b"--frame\r\nContent-Type: image/jpeg\r\n\r\nJPEG\r\n").await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        (url, connections, cancel, task)
    }

    async fn devicekit_h264_source() -> (String, CancellationToken, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    () = task_cancel.cancelled() => return,
                    accepted = listener.accept() => accepted,
                };
                let (mut socket, _) = accepted.unwrap();
                let connection_cancel = task_cancel.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    loop {
                        let count = socket.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    if request.starts_with("GET /health HTTP/1.1") {
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                            )
                            .await
                            .unwrap();
                        return;
                    }
                    assert!(request.starts_with("GET /h264?bitrate=4000000 HTTP/1.1"));
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: video/h264\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                    let mut ticker = tokio::time::interval(Duration::from_millis(10));
                    loop {
                        tokio::select! {
                            () = connection_cancel.cancelled() => return,
                            _ = ticker.tick() => {
                                if socket.write_all(b"\x00\x00\x00\x01\x65H264").await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        (url, cancel, task)
    }

    fn ui_stream_spec(base_url: String) -> UiStreamSpec {
        UiStreamSpec {
            driver: UiAutomationDriver::Wda,
            format: UiStreamFormat::Mjpeg,
            wda_url: base_url,
            devicekit_url: "http://127.0.0.1:12004".into(),
            wda_task_id: None,
            output_path: None,
            fps: Some(30),
            quality: Some(80),
            scale: None,
            bitrate: None,
        }
    }

    #[tokio::test]
    async fn forward_task_echoes_and_releases_port() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let record = manager.start_forward(forward_spec()).await.unwrap();
        let TaskSpec::Forward(spec) = &record.spec else {
            panic!("forward spec expected");
        };
        let port = spec.host_port;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"forward-data").await.unwrap();
        let mut bytes = [0_u8; 12];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"forward-data");
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("port should be released");
    }

    #[tokio::test]
    async fn forward_is_restored_after_manager_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &path).await;
        let record = first.start_forward(forward_spec()).await.unwrap();
        let TaskSpec::Forward(spec) = &record.spec else {
            panic!("forward spec expected");
        };
        let port = spec.host_port;
        first.shutdown().await;
        drop(first);

        let second = manager(backend, &path).await;
        wait_state(&second, record.id, RuntimeState::Running).await;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"restored").await.unwrap();
        let mut bytes = [0_u8; 8];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"restored");
        second.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn dproxy_captures_restores_restarts_and_releases_listener() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let output = directory.path().join("dproxy-events.ndjson");
        let (upstream, upstream_cancel, upstream_task) = fake_usbmuxd_upstream().await;
        let backend = MockDeviceBackend::new();
        let first = manager(backend.clone(), &state).await;
        let record = first
            .start_dproxy(dproxy_spec(upstream, &output))
            .await
            .unwrap();
        let TaskSpec::Dproxy(spec) = &record.spec else {
            panic!("dproxy spec expected");
        };
        let port = spec.port;
        let packet = usbmuxd_list_devices_packet();
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(&packet).await.unwrap();
        let mut echoed = vec![0_u8; packet.len()];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, packet);
        client.shutdown().await.unwrap();
        drop(client);
        let event_output = wait_output_contains(&first, record.id, "connection_closed").await;
        assert_eq!(event_output.content_type, "application/x-ndjson");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let running = loop {
            let current = first.show(record.id).await.unwrap();
            if current.runtime["connectionsCompleted"] == 1 {
                break current;
            }
            assert!(tokio::time::Instant::now() < deadline, "{current:#?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(running.runtime["connectionsAccepted"], 1);
        assert_eq!(running.runtime["connectionsCompleted"], 1);
        assert_eq!(running.runtime["hostToUsbmuxdBytes"], packet.len() as u64);
        let capture_root = PathBuf::from(running.runtime["captureDirectory"].as_str().unwrap());
        let capture = capture_root.join("generation-1").join("connection-1");
        assert_eq!(
            tokio::fs::read(capture.join("host-to-usbmuxd.bin"))
                .await
                .unwrap(),
            packet
        );
        let decoded = tokio::fs::read_to_string(capture.join("host-to-usbmuxd.ndjson"))
            .await
            .unwrap();
        assert!(decoded.contains("ListDevices"));
        assert!(decoded.contains("generation-1-connection-1"));

        first.shutdown().await;
        drop(first);
        let second = manager(backend, &state).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.runtime["port"], port);
        let restarted = second.restart(record.id).await.unwrap();
        assert_eq!(restarted.generation, restored.generation + 1);
        wait_state(&second, record.id, RuntimeState::Running).await;
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(b"after-restart").await.unwrap();
        let mut echoed = [0_u8; 13];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"after-restart");
        client.shutdown().await.unwrap();
        drop(client);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let current = second.show(record.id).await.unwrap();
            if current.runtime["connectionsCompleted"] == 1 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{current:#?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut active = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        active.write_all(b"active").await.unwrap();
        let mut echoed = [0_u8; 6];
        active.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"active");
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), active.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("dproxy port should be released");
        upstream_cancel.cancel();
        upstream_task.await.unwrap();
    }

    #[tokio::test]
    async fn dproxy_rejects_invalid_or_unimplemented_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(
            MockDeviceBackend::new(),
            &directory.path().join("state.json"),
        )
        .await;
        let output = directory.path().join("dproxy.ndjson");
        let upstream = "127.0.0.1:27015".parse().unwrap();

        let mut utun = dproxy_spec(upstream, &output);
        utun.mode = DproxyMode::Utun;
        assert!(matches!(
            manager.start_dproxy(utun).await,
            Err(TaskManagerError::Core(CoreError::NotImplemented(_)))
        ));

        let mut interface = dproxy_spec(upstream, &output);
        interface.interface = Some("utun9".into());
        assert!(matches!(
            manager.start_dproxy(interface).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let mut malformed = dproxy_spec(upstream, &output);
        malformed.upstream_address = "localhost:27015".into();
        assert!(matches!(
            manager.start_dproxy(malformed).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let mut relative = dproxy_spec(upstream, &output);
        relative.output_path = Some("relative.ndjson".into());
        assert!(matches!(
            manager.start_dproxy(relative).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let available = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = available.local_addr().unwrap().port();
        drop(available);
        let mut self_proxy = dproxy_spec(format!("127.0.0.1:{port}").parse().unwrap(), &output);
        self_proxy.port = port;
        assert!(matches!(
            manager.start_dproxy(self_proxy).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let mut exposed = dproxy_spec(upstream, &output);
        exposed.bind_host = "0.0.0.0".into();
        assert!(matches!(
            manager.start_dproxy(exposed).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));
    }

    #[tokio::test]
    async fn location_simulation_completes_once_and_requires_explicit_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let source = directory.path().join("route.gpx");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let record = first
            .start_location_simulation(location_simulation_spec(&source))
            .await
            .unwrap();
        let completed = wait_state(&first, record.id, RuntimeState::Completed).await;
        assert_eq!(completed.kind, TaskKind::LocationSimulation);
        assert_eq!(completed.desired_state, DesiredState::Stopped);
        assert_eq!(completed.restart_policy, RestartPolicy::Never);
        assert_eq!(completed.runtime["completedPoints"], 2);
        assert_eq!(completed.runtime["serviceTransport"], "mock");
        assert_eq!(backend.location_session_count(), 1);
        assert_eq!(backend.location_points().await.len(), 2);
        assert_eq!(backend.location_clear_count(), 0);
        let output = wait_output(&first, record.id, 1).await;
        let text = String::from_utf8(BASE64.decode(output.bytes_base64).unwrap()).unwrap();
        assert!(text.contains("\"simulation_started\""));
        assert_eq!(text.matches("\"location_set\"").count(), 2);
        assert!(text.contains("\"simulation_completed\""));
        assert_eq!(output.content_type, "application/x-ndjson");

        first.shutdown().await;
        drop(first);
        let second = manager(backend.clone(), &state).await;
        let restored = second.show(record.id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Completed);
        assert_eq!(backend.location_session_count(), 1);
        second.restart(record.id).await.unwrap();
        let restarted = wait_state(&second, record.id, RuntimeState::Completed).await;
        assert_eq!(restarted.generation, 2);
        assert_eq!(backend.location_session_count(), 2);
        assert_eq!(backend.location_points().await.len(), 4);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_location_simulation_clears_the_partial_route() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let mut spec = location_simulation_spec(&directory.path().join("long-route.gpx"));
        spec.points[1].delay_before_ms = 10_000;
        let record = manager.start_location_simulation(spec).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Running).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while backend.location_points().await.is_empty() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output_path = PathBuf::from(
            manager.show(record.id).await.unwrap().runtime["outputPath"]
                .as_str()
                .unwrap(),
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.location_points().await.len(), 1);
        assert_eq!(backend.location_clear_count(), 1);
        let text = tokio::fs::read_to_string(output_path).await.unwrap();
        assert!(text.contains("\"simulation_cancelled\""));
    }

    #[tokio::test]
    async fn device_state_is_disabled_and_restored_with_server() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &path).await;
        let record = first
            .start_device_state(DeviceStateSpec {
                udid: Some("mock-phone".into()),
                condition_identifier: "SlowNetworkCondition".into(),
                profile_identifier: "SlowNetwork3GGood".into(),
            })
            .await
            .unwrap();
        let running = wait_state(&first, record.id, RuntimeState::Running).await;
        assert_eq!(running.kind, TaskKind::DeviceState);
        assert_eq!(backend.condition_count(), 1);
        first.shutdown().await;
        assert_eq!(backend.condition_close_count(), 1);
        drop(first);

        let second = manager(backend.clone(), &path).await;
        wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(backend.condition_count(), 2);
        second.shutdown().await;
        assert_eq!(backend.condition_close_count(), 2);
    }

    #[tokio::test]
    async fn screenshot_stream_serves_multipart_and_releases_port() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let record = manager
            .start_screenshot_stream(ScreenshotStreamSpec {
                udid: Some("mock-phone".into()),
                bind_host: "127.0.0.1".into(),
                port: 0,
                interval_ms: 20,
            })
            .await
            .unwrap();
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        let endpoint = running.runtime["endpoint"].as_str().unwrap().to_owned();
        let mut client = TcpStream::connect(&endpoint).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(3), async {
            let mut response = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let count = client.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                response.extend_from_slice(&chunk[..count]);
                if response.windows(2).any(|window| window == [0xff, 0xd8]) {
                    return response;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            String::from_utf8_lossy(&response)
                .contains("multipart/x-mixed-replace; boundary=BoundaryString")
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        TcpListener::bind(&endpoint).await.unwrap();
    }

    #[tokio::test]
    async fn ui_stream_captures_output_and_restores_after_manager_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        let (url, connections, source_cancel, source_task) =
            ui_stream_source("/mjpeg?fps=30&quality=80").await;
        let first = manager(backend.clone(), &path).await;
        let record = first.start_ui_stream(ui_stream_spec(url)).await.unwrap();
        let running = wait_state(&first, record.id, RuntimeState::Running).await;
        assert_eq!(running.kind, TaskKind::UiStream);
        assert_eq!(running.runtime["resolvedDriver"], "wda");
        assert_eq!(running.runtime["format"], "mjpeg");
        let first_output = wait_output(&first, record.id, 32).await;
        assert_eq!(
            first_output.content_type,
            "multipart/x-mixed-replace; boundary=frame"
        );
        assert!(
            BASE64
                .decode(&first_output.bytes_base64)
                .unwrap()
                .windows(4)
                .any(|window| window == b"JPEG")
        );
        first.shutdown().await;
        drop(first);

        let second = manager(backend, &path).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.generation, 2);
        let second_output = wait_output(&second, record.id, first_output.total_bytes + 1).await;
        assert!(second_output.total_bytes > first_output.total_bytes);
        assert!(connections.load(Ordering::SeqCst) >= 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        source_cancel.cancel();
        source_task.await.unwrap();
    }

    #[tokio::test]
    async fn ui_stream_tracks_referenced_wda_task_url_changes() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let (first_url, _, first_cancel, first_source) =
            ui_stream_source("/?fps=30&quality=80").await;
        let (second_url, second_connections, second_cancel, second_source) =
            ui_stream_source("/?fps=30&quality=80").await;

        let fake_wda = manager
            .insert_record(new_record(TaskSpec::Wda(Box::new(wda_spec()))))
            .await;
        let fake_wda_id = fake_wda.record.read().await.id;
        set_state(
            &fake_wda.record,
            RuntimeState::Running,
            Some("fake WDA ready".into()),
            json!({ "wdaUrl": first_url, "mjpegUrl": first_url }),
        )
        .await;

        let record = manager
            .start_ui_stream(ui_stream_spec(first_url.clone()))
            .await
            .unwrap();
        let TaskSpec::UiStream(spec) = &record.spec else {
            panic!("UI stream spec expected");
        };
        assert_eq!(spec.wda_task_id, Some(fake_wda_id));
        let first_output = wait_output(&manager, record.id, 32).await;

        first_cancel.cancel();
        first_source.await.unwrap();
        set_state(
            &fake_wda.record,
            RuntimeState::Running,
            Some("fake WDA moved".into()),
            json!({ "wdaUrl": second_url, "mjpegUrl": second_url }),
        )
        .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let current = manager.show(record.id).await.unwrap();
            if current.runtime_state == RuntimeState::Running
                && current.runtime["streamUrl"]
                    .as_str()
                    .is_some_and(|url| url.starts_with(&second_url))
                && current.runtime["bytesWritten"].as_u64().unwrap_or(0) > first_output.total_bytes
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "last record: {current:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(second_connections.load(Ordering::SeqCst) >= 1);
        manager.stop(TaskSelector::All).await.unwrap();
        second_cancel.cancel();
        second_source.await.unwrap();
    }

    #[tokio::test]
    async fn ui_stream_auto_selects_devicekit_for_h264() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let (url, source_cancel, source_task) = devicekit_h264_source().await;
        let record = manager
            .start_ui_stream(UiStreamSpec {
                driver: UiAutomationDriver::Auto,
                format: UiStreamFormat::H264,
                wda_url: "http://127.0.0.1:1".into(),
                devicekit_url: url,
                wda_task_id: Some(Uuid::new_v4()),
                output_path: None,
                fps: None,
                quality: None,
                scale: None,
                bitrate: Some(4_000_000),
            })
            .await
            .unwrap();
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        assert_eq!(running.runtime["resolvedDriver"], "devicekit");
        assert_eq!(running.runtime["format"], "h264");
        let output = wait_output(&manager, record.id, 9).await;
        assert_eq!(output.content_type, "video/h264");
        assert!(
            BASE64
                .decode(output.bytes_base64)
                .unwrap()
                .windows(5)
                .any(|window| window == b"\x00\x00\x00\x01\x65")
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        source_cancel.cancel();
        source_task.await.unwrap();
    }

    #[tokio::test]
    async fn webinspector_cdp_serves_discovery_bridges_commands_and_releases_resources() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager
            .start_webinspector_cdp(webinspector_cdp_spec())
            .await
            .unwrap();
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        let endpoint = running.runtime["endpoint"].as_str().unwrap().to_owned();

        let targets = reqwest::get(format!("http://{endpoint}/json"))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(targets[0]["id"], "mock-page-1");
        assert_eq!(targets[0]["type"], "page");
        let websocket_url = targets[0]["webSocketDebuggerUrl"]
            .as_str()
            .unwrap()
            .to_owned();
        let (mut websocket, _) = tokio_tungstenite::connect_async(&websocket_url)
            .await
            .unwrap();
        websocket
            .send(WebSocketMessage::Text(
                json!({ "id": 17, "method": "Runtime.enable" })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let WebSocketMessage::Text(response) = response else {
            panic!("CDP JSON text response expected");
        };
        let response: Value = serde_json::from_str(response.as_str()).unwrap();
        assert_eq!(response["id"], 17);
        assert_eq!(response["result"]["method"], "Runtime.enable");

        let second = tokio_tungstenite::connect_async(&websocket_url).await;
        let Err(WebSocketError::Http(response)) = second else {
            panic!("second active page WebSocket should be rejected");
        };
        assert_eq!(response.status(), StatusCode::CONFLICT);

        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.webinspector_cdp_count(), 1);
        assert_eq!(backend.webinspector_cdp_close_count(), 1);
        TcpListener::bind(&endpoint)
            .await
            .expect("CDP port should be released");
    }

    #[tokio::test]
    async fn webinspector_cdp_and_js_shell_are_mutually_exclusive_per_device() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;

        // CDP 运行时启动 js-shell：必须立即 conflict，而不是等设备侧超时
        // （2026-08-10 真机观察到占用时 20 秒超时）。
        let cdp = manager
            .start_webinspector_cdp(webinspector_cdp_spec())
            .await
            .unwrap();
        wait_state(&manager, cdp.id, RuntimeState::Running).await;
        let conflict = manager
            .start_webinspector_js_shell(webinspector_js_shell_spec())
            .await
            .unwrap_err();
        assert!(
            matches!(&conflict, TaskManagerError::Conflict(message) if message.contains("already has WebInspector CDP task")),
            "expected Conflict, got {conflict:?}"
        );
        manager.stop(TaskSelector::Id(cdp.id)).await.unwrap();

        // js-shell 运行时启动 CDP：同样立即 conflict。
        let shell = manager
            .start_webinspector_js_shell(webinspector_js_shell_spec())
            .await
            .unwrap();
        wait_state(&manager, shell.id, RuntimeState::Running).await;
        let conflict = manager
            .start_webinspector_cdp(webinspector_cdp_spec())
            .await
            .unwrap_err();
        assert!(
            matches!(&conflict, TaskManagerError::Conflict(message) if message.contains("already has WebInspector JS shell task")),
            "expected Conflict, got {conflict:?}"
        );
        manager.stop(TaskSelector::Id(shell.id)).await.unwrap();
    }

    #[tokio::test]
    async fn webinspector_js_shell_evaluates_captures_errors_and_releases_session() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager
            .start_webinspector_js_shell(webinspector_js_shell_spec())
            .await
            .unwrap();
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        assert_eq!(running.kind, TaskKind::WebInspectorJsShell);
        assert_eq!(
            running.runtime["page"]["bundle_id"],
            "com.apple.mobilesafari"
        );
        let result = manager
            .webinspector_js_shell_eval(record.id, "2 + 2".into())
            .await
            .unwrap();
        assert_eq!(result.value, Some(json!(4)));
        let error = manager
            .webinspector_js_shell_eval(record.id, "throw new Error('boom')".into())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            TaskManagerError::Core(CoreError::Backend(_))
        ));
        assert_eq!(
            manager.show(record.id).await.unwrap().runtime_state,
            RuntimeState::Running
        );
        let output = manager.read_output(record.id, 0, 65_536).await.unwrap();
        let bytes = BASE64.decode(output.bytes_base64).unwrap();
        let lines: Vec<_> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["result"], 4);
        assert!(lines[1]["error"].as_str().unwrap().contains("exception"));

        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.webinspector_js_shell_count(), 1);
        assert_eq!(backend.webinspector_js_shell_close_count(), 1);
    }

    #[tokio::test]
    async fn interrupted_webinspector_js_shell_is_failed_without_replaying_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &path).await;
        let record = first
            .start_webinspector_js_shell(webinspector_js_shell_spec())
            .await
            .unwrap();
        wait_state(&first, record.id, RuntimeState::Running).await;
        first
            .webinspector_js_shell_eval(record.id, "2 + 2".into())
            .await
            .unwrap();
        first.shutdown().await;
        drop(first);

        let second = manager(backend.clone(), &path).await;
        let restored = second.show(record.id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Failed);
        assert_eq!(restored.desired_state, DesiredState::Stopped);
        assert!(restored.message.unwrap().contains("interrupted"));
        assert_eq!(backend.webinspector_js_shell_count(), 1);
        assert_eq!(backend.webinspector_js_shell_close_count(), 1);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn webinspector_cdp_is_restored_after_manager_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &path).await;
        let record = first
            .start_webinspector_cdp(webinspector_cdp_spec())
            .await
            .unwrap();
        let running = wait_state(&first, record.id, RuntimeState::Running).await;
        let endpoint = running.runtime["endpoint"].as_str().unwrap().to_owned();
        first.shutdown().await;
        assert_eq!(backend.webinspector_cdp_close_count(), 1);
        drop(first);

        let second = manager(backend.clone(), &path).await;
        wait_state(&second, record.id, RuntimeState::Running).await;
        let targets = reqwest::get(format!("http://{endpoint}/json/list"))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(targets[0]["id"], "mock-page-1");
        assert_eq!(backend.webinspector_cdp_count(), 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.webinspector_cdp_close_count(), 2);
    }

    #[tokio::test]
    async fn tunnel_waits_for_device_then_serves_compat_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Waiting).await;
        backend.attach_usb("mock-phone").await;
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        let endpoint = running.runtime["userspaceEndpoint"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut stream = TcpStream::connect(endpoint).await.unwrap();
        stream
            .write_all(&Ipv6Addr::LOCALHOST.octets())
            .await
            .unwrap();
        stream.write_all(&8100_u32.to_le_bytes()).await.unwrap();
        stream.write_all(b"tunnel").await.unwrap();
        let mut bytes = [0_u8; 6];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"tunnel");
        let rsd = manager
            .execute(&CommandInvocation {
                argv: vec!["rsd".into(), "ls".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: rust_ios_protocol::OutputOptions::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            rsd.value.unwrap()["com.apple.internal.dt.remote.debugproxy"]["Port"],
            58_785
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        let error = manager
            .execute(&CommandInvocation {
                argv: vec!["rsd".into(), "ls".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: rust_ios_protocol::OutputOptions::default(),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no running userspace tunnel"));
    }

    #[tokio::test]
    async fn userspace_tunnel_restarts_after_data_plane_failure() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Running).await;

        backend.set_connect_failure(true);
        // 探针迟滞（同步 fork 3172596）：连续 PROBE_FAILURES_BEFORE_REBUILD=3 次
        // 失败（2s 间隔）才进入 BackingOff。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let record = manager.show(record.id).await.unwrap();
            if record.runtime_state == RuntimeState::BackingOff {
                assert!(
                    record
                        .message
                        .as_deref()
                        .unwrap()
                        .contains("data-plane health check failed")
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "last record: {record:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        backend.set_connect_failure(false);
        let restored = wait_state(&manager, record.id, RuntimeState::Running).await;
        assert!(restored.attempts >= 2);
        assert!(backend.tunnel_count() >= 2);
        let endpoint = restored.runtime["userspaceEndpoint"].as_str().unwrap();
        let mut stream = TcpStream::connect(endpoint).await.unwrap();
        stream
            .write_all(&Ipv6Addr::LOCALHOST.octets())
            .await
            .unwrap();
        stream.write_all(&8100_u32.to_le_bytes()).await.unwrap();
        stream.write_all(b"restored").await.unwrap();
        let mut bytes = [0_u8; 8];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"restored");

        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn userspace_tunnel_survives_transient_data_plane_failure() {
        // 探针迟滞：少于阈值的连续失败只告警并保留隧道，不触发重建。
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Running).await;

        // 2.5s 保证至少一次探针失败（间隔 2s），但少于 3 次阈值。
        backend.set_connect_failure(true);
        tokio::time::sleep(Duration::from_millis(2500)).await;
        backend.set_connect_failure(false);

        // 数据面恢复：隧道从未重建（attempt/tunnel 计数不变）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let record = manager.show(record.id).await.unwrap();
            let running = record.runtime_state == RuntimeState::Running;
            let tunnels = backend.tunnel_count();
            if running && tunnels == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "tunnel did not stay up: state {:?}, tunnels {tunnels}",
                record.runtime_state
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            manager.show(record.id).await.unwrap().attempts,
            1,
            "a transient probe failure must not rebuild the tunnel"
        );

        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn kernel_tunnel_waits_registers_route_and_has_no_userspace_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let record = manager.start_tunnel(kernel_tunnel_spec()).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Waiting).await;
        backend.attach_usb("mock-phone").await;
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        assert_eq!(running.runtime["kernel"], true);
        assert_eq!(running.runtime["userspace"], false);
        assert_eq!(running.runtime["interfaceName"], "rust-ios-mock-phone");
        assert!(running.runtime.get("userspaceEndpoint").is_none());

        let rsd = manager
            .execute(&CommandInvocation {
                argv: vec!["rsd".into(), "ls".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(
            rsd.value.unwrap()["com.apple.internal.dt.remote.debugproxy"]["Port"],
            58_785
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.kernel_tunnel_count(), 1);
        assert_eq!(backend.kernel_tunnel_close_count(), 1);
    }

    #[tokio::test]
    async fn kernel_tunnel_is_closed_and_restored_with_server_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let record = first.start_tunnel(kernel_tunnel_spec()).await.unwrap();
        wait_state(&first, record.id, RuntimeState::Running).await;
        first.shutdown().await;
        assert_eq!(backend.kernel_tunnel_count(), 1);
        assert_eq!(backend.kernel_tunnel_close_count(), 1);
        drop(first);

        let second = manager(backend.clone(), &state).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.runtime["interfaceName"], "rust-ios-mock-phone");
        assert_eq!(backend.kernel_tunnel_count(), 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.kernel_tunnel_close_count(), 2);
    }

    #[tokio::test]
    async fn kernel_tunnel_rejects_remote_relative_and_userspace_only_options() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager(
            MockDeviceBackend::new(),
            &directory.path().join("state.json"),
        )
        .await;

        let mut remote = kernel_tunnel_spec();
        remote.transport = TunnelTransport::RemotePairingTcp;
        assert!(matches!(
            manager.start_tunnel(remote).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let mut relative = kernel_tunnel_spec();
        relative.wintun_path = Some("wintun.dll".into());
        assert!(matches!(
            manager.start_tunnel(relative).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));

        let mut userspace = tunnel_spec();
        userspace.kernel_interface_name = Some("not-allowed".into());
        assert!(matches!(
            manager.start_tunnel(userspace).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));
    }

    #[tokio::test]
    async fn debug_proxy_reuses_tunnel_and_releases_listener() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let tunnel = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, tunnel.id, RuntimeState::Running).await;
        let debug_proxy = manager
            .start_debug_proxy(DebugProxySpec {
                udid: Some("mock-phone".into()),
                bind_host: "127.0.0.1".into(),
                port: 0,
                client_owned: false,
            })
            .await
            .unwrap();
        let running = wait_state(&manager, debug_proxy.id, RuntimeState::Running).await;
        let endpoint = running.runtime["endpoint"].as_str().unwrap().to_owned();
        let mut client = TcpStream::connect(&endpoint).await.unwrap();
        client.write_all(b"gdb-remote").await.unwrap();
        let mut response = [0_u8; 10];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"gdb-remote");

        manager
            .stop(TaskSelector::Id(debug_proxy.id))
            .await
            .unwrap();
        TcpListener::bind(&endpoint).await.unwrap();
        manager.stop(TaskSelector::Id(tunnel.id)).await.unwrap();
    }

    #[tokio::test]
    async fn debug_proxy_waits_until_rsd_service_is_advertised() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        backend.set_debug_proxy_available(false);
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let tunnel = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, tunnel.id, RuntimeState::Running).await;
        let debug_proxy = manager
            .start_debug_proxy(DebugProxySpec {
                udid: Some("mock-phone".into()),
                bind_host: "127.0.0.1".into(),
                port: 0,
                client_owned: false,
            })
            .await
            .unwrap();
        let waiting = wait_state(&manager, debug_proxy.id, RuntimeState::Waiting).await;
        assert!(
            waiting
                .message
                .as_deref()
                .unwrap()
                .contains("is not advertised")
        );

        backend.set_debug_proxy_available(true);
        wait_state(&manager, debug_proxy.id, RuntimeState::Running).await;
        manager
            .stop(TaskSelector::Id(debug_proxy.id))
            .await
            .unwrap();
        manager.stop(TaskSelector::Id(tunnel.id)).await.unwrap();
    }

    #[tokio::test]
    async fn shared_rsd_commands_and_streams_reuse_active_tunnel() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;

        let tunnel = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, tunnel.id, RuntimeState::Running).await;

        let result = manager
            .execute(&CommandInvocation {
                argv: vec!["ps".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );

        let webinspector = manager
            .execute(&CommandInvocation {
                argv: vec!["webinspector".into(), "list".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert!(
            webinspector
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );

        let screenshot_path = directory.path().join("screenshot.png");
        let screenshot = manager
            .execute(&CommandInvocation {
                argv: vec![
                    "screenshot".into(),
                    format!("--output={}", screenshot_path.display()),
                ],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert!(screenshot_path.is_file());
        assert!(
            screenshot
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );

        let location = manager
            .execute(&CommandInvocation {
                argv: vec![
                    "setlocation".into(),
                    "--lat=31.2304".into(),
                    "--lon=121.4737".into(),
                ],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert!(
            location
                .warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );

        let file = manager
            .execute(&CommandInvocation {
                argv: vec!["file".into(), "ls".into(), "--temp".into()],
                cwd: None,
                udid: Some("mock-phone".into()),
                output: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(file.value.unwrap()["serviceTransport"], "shared_rsd");
        assert!(
            file.warnings
                .iter()
                .any(|warning| warning.contains("shared RSD"))
        );

        let stream = manager
            .start_stream(stream_spec(
                TaskKind::Notifications,
                StreamFormat::JsonLines,
            ))
            .await
            .unwrap();
        let running = wait_state(&manager, stream.id, RuntimeState::Running).await;
        assert_eq!(running.runtime["serviceTransport"], "shared_rsd");

        manager.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn wda_session_is_server_owned_and_restored() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let tunnel = first.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&first, tunnel.id, RuntimeState::Running).await;
        let record = first.start_wda(wda_spec()).await.unwrap();
        let running = wait_state(&first, record.id, RuntimeState::Running).await;
        assert_eq!(running.kind, TaskKind::Wda);
        assert_eq!(running.runtime["wdaUrl"], "http://127.0.0.1:18100");
        assert_eq!(running.runtime["contentType"], "application/x-ndjson");
        assert_eq!(running.runtime["serviceTransport"], "shared_rsd");
        assert_eq!(running.runtime["eventsWritten"], 1);
        let output_path = PathBuf::from(running.runtime["outputPath"].as_str().unwrap());
        assert!(output_path.starts_with(directory.path().join("captures")));
        let output = first.read_output(record.id, 0, 65_536).await.unwrap();
        assert_eq!(output.content_type, "application/x-ndjson");
        let first_capture = BASE64.decode(output.bytes_base64).unwrap();
        assert_eq!(
            first_capture.iter().filter(|byte| **byte == b'\n').count(),
            1
        );
        assert_eq!(backend.wda_count(), 1);
        first.shutdown().await;
        assert_eq!(backend.wda_close_count(), 1);
        drop(first);

        let second = manager(backend.clone(), &state).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.runtime["eventsWritten"], 1);
        assert_eq!(restored.runtime["serviceTransport"], "shared_rsd");
        let output = second.read_output(record.id, 0, 65_536).await.unwrap();
        let restored_capture = BASE64.decode(output.bytes_base64).unwrap();
        assert_eq!(
            restored_capture
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            2
        );
        assert_eq!(backend.wda_count(), 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.wda_close_count(), 2);
        second.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn ui_runner_forwards_health_echoes_logs_and_releases_resources() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let tunnel = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, tunnel.id, RuntimeState::Running).await;
        let record = manager
            .start_ui_runner(ui_runner_spec(UiRunnerTarget::Wda))
            .await
            .unwrap();
        let running = wait_state(&manager, record.id, RuntimeState::Running).await;
        assert_eq!(running.kind, TaskKind::UiRunner);
        assert_eq!(running.runtime["target"], "wda");
        assert_eq!(running.runtime["devicePort"], 8100);
        assert_eq!(running.runtime["serviceTransport"], "shared_rsd");
        assert_eq!(running.runtime["health"]["ready"], true);
        assert_eq!(backend.ui_runner_count(), 1);
        let port = running.runtime["hostPort"].as_u64().unwrap() as u16;

        let health = reqwest::get(format!("http://127.0.0.1:{port}/status"))
            .await
            .unwrap();
        assert!(health.status().is_success());
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(b"ui-runner-echo").await.unwrap();
        let mut echoed = [0_u8; 14];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ui-runner-echo");

        let output = wait_output(&manager, record.id, 1).await;
        let text = String::from_utf8(BASE64.decode(output.bytes_base64).unwrap()).unwrap();
        assert!(text.contains("mock UI runner started"));
        assert_eq!(output.content_type, "application/x-ndjson");

        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.ui_runner_close_count(), 1);
        let rebound = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        drop(rebound);
        manager.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn ui_runner_is_restored_with_the_same_endpoint() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let record = first
            .start_ui_runner(ui_runner_spec(UiRunnerTarget::DeviceKit))
            .await
            .unwrap();
        let running = wait_state(&first, record.id, RuntimeState::Running).await;
        let port = running.runtime["hostPort"].as_u64().unwrap() as u16;
        first.shutdown().await;
        assert_eq!(backend.ui_runner_close_count(), 1);
        drop(first);

        let second = manager(backend.clone(), &state).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.generation, 2);
        assert_eq!(restored.runtime["hostPort"], port);
        assert_eq!(restored.runtime["target"], "devicekit");
        assert_eq!(backend.ui_runner_count(), 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.ui_runner_close_count(), 2);
    }

    #[tokio::test]
    async fn xctest_completes_once_persists_output_and_requires_explicit_restart() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let tunnel = first.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&first, tunnel.id, RuntimeState::Running).await;
        let record = first.start_xctest(xctest_spec()).await.unwrap();
        let completed = wait_state(&first, record.id, RuntimeState::Completed).await;
        assert_eq!(completed.kind, TaskKind::XCTest);
        assert_eq!(completed.desired_state, DesiredState::Stopped);
        assert_eq!(completed.restart_policy, RestartPolicy::Never);
        assert_eq!(completed.runtime["outcome"], "passed");
        assert_eq!(completed.runtime["summary"]["passed"], 1);
        assert_eq!(completed.runtime["serviceTransport"], "shared_rsd");
        assert_eq!(backend.xctest_count(), 1);
        let output = wait_output(&first, record.id, 1).await;
        let bytes = BASE64.decode(output.bytes_base64).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"case_finished\""));
        assert_eq!(output.content_type, "application/x-ndjson");
        first.shutdown().await;
        drop(first);

        let second = manager(backend.clone(), &state).await;
        let restored = second.show(record.id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Completed);
        assert_eq!(backend.xctest_count(), 1, "finite task must not auto-run");

        second.restart(record.id).await.unwrap();
        let restarted = wait_state(&second, record.id, RuntimeState::Completed).await;
        assert_eq!(restarted.generation, 2);
        assert_eq!(restarted.runtime["serviceTransport"], "shared_rsd");
        assert_eq!(backend.xctest_count(), 2);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
        second.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn app_process_captures_output_and_only_restarts_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let first = manager(backend.clone(), &state).await;
        let tunnel = first.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&first, tunnel.id, RuntimeState::Running).await;
        let record = first.start_app_launch(app_launch_spec()).await.unwrap();
        let completed = wait_state(&first, record.id, RuntimeState::Completed).await;
        assert_eq!(completed.kind, TaskKind::AppProcess);
        assert_eq!(completed.desired_state, DesiredState::Stopped);
        assert_eq!(completed.restart_policy, RestartPolicy::Never);
        assert_eq!(completed.runtime["pid"], 42_424);
        assert_eq!(completed.runtime["serviceTransport"], "shared_rsd");
        assert_eq!(backend.app_process_count(), 1);
        assert_eq!(backend.app_process_close_count(), 0);
        let output = wait_output(&first, record.id, 1).await;
        let bytes = BASE64.decode(output.bytes_base64).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "mock app stdout\nmock app stderr\n"
        );
        assert_eq!(output.content_type, "text/plain; charset=utf-8");
        first.shutdown().await;
        drop(first);

        let second = manager(backend.clone(), &state).await;
        let restored = second.show(record.id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Completed);
        assert_eq!(
            backend.app_process_count(),
            1,
            "finite app task must not auto-run"
        );
        second.restart(record.id).await.unwrap();
        let restarted = wait_state(&second, record.id, RuntimeState::Completed).await;
        assert_eq!(restarted.generation, 2);
        assert_eq!(backend.app_process_count(), 2);
        second.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn stopping_app_process_closes_owned_session() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend.clone(), &directory.path().join("state.json")).await;
        let tunnel = manager.start_tunnel(tunnel_spec()).await.unwrap();
        wait_state(&manager, tunnel.id, RuntimeState::Running).await;
        let record = manager.start_app_launch(app_launch_spec()).await.unwrap();
        assert_eq!(record.runtime_state, RuntimeState::Running);
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
        assert_eq!(backend.app_process_close_count(), 1);
        manager.stop(TaskSelector::Id(tunnel.id)).await.unwrap();
    }

    #[tokio::test]
    async fn unit_xctest_completes_with_an_explicit_test_host_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let mut spec = xctest_spec();
        spec.targets[0].is_ui_test = false;
        spec.targets[0].name = "DemoUnitTests".into();
        spec.targets[0].runner_bundle_id = "com.example.Demo".into();
        spec.targets[0].target_bundle_id = None;
        spec.targets[0].xctest_config = Some("DemoUnitTests.xctest".into());
        let record = manager.start_xctest(spec).await.unwrap();
        let completed = wait_state(&manager, record.id, RuntimeState::Completed).await;
        assert_eq!(completed.runtime["outcome"], "passed");
        assert_eq!(completed.runtime["summary"]["passed"], 1);
    }

    #[tokio::test]
    async fn xctest_filters_select_the_mock_case_in_captured_output() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let mut spec = xctest_spec();
        spec.targets[0].tests_to_run = vec!["DemoUITests.DemoCase/testSelected".into()];
        spec.targets[0].tests_to_skip = vec!["DemoUITests.DemoCase/testSkipped".into()];
        let record = manager.start_xctest(spec).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Completed).await;
        let output = wait_output(&manager, record.id, 1).await;
        let text = String::from_utf8(BASE64.decode(output.bytes_base64).unwrap()).unwrap();
        assert!(matches!(
            serde_json::from_str::<Value>(text.lines().next().unwrap()),
            Ok(Value::Object(_))
        ));
        assert!(text.contains("\"test_class\":\"DemoCase\""));
        assert!(text.contains("\"method\":\"testSelected\""));
    }

    #[tokio::test]
    async fn unit_xctest_without_an_explicit_bundle_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let mut spec = xctest_spec();
        spec.targets[0].is_ui_test = false;
        spec.targets[0].target_bundle_id = None;
        spec.targets[0].xctest_config = None;
        let error = manager.start_xctest(spec).await.unwrap_err();
        assert!(matches!(
            error,
            TaskManagerError::Core(CoreError::InvalidArgument(_))
        ));

        let mut spec = xctest_spec();
        spec.targets[0].is_ui_test = false;
        spec.targets[0].target_bundle_id = None;
        spec.targets[0].xctest_config = Some(".xctest".into());
        let error = manager.start_xctest(spec).await.unwrap_err();
        assert!(matches!(
            error,
            TaskManagerError::Core(CoreError::InvalidArgument(_))
        ));

        let mut spec = xctest_spec();
        spec.targets[0].is_ui_test = false;
        spec.targets[0].target_bundle_id = Some(spec.targets[0].runner_bundle_id.clone());
        let error = manager.start_xctest(spec).await.unwrap_err();
        assert!(matches!(
            error,
            TaskManagerError::Core(CoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn implicit_and_explicit_device_selectors_conflict_for_wda_and_xctest() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let mut wda = wda_spec();
        wda.udid = None;
        manager.start_wda(wda).await.unwrap();

        let error = manager.start_xctest(xctest_spec()).await.unwrap_err();
        assert!(matches!(error, TaskManagerError::Conflict(_)));
        manager.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn ui_runner_conflicts_with_wda_and_rejects_target_transport_override() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;

        let record = manager
            .start_ui_runner(ui_runner_spec(UiRunnerTarget::Wda))
            .await
            .unwrap();
        let error = manager.start_wda(wda_spec()).await.unwrap_err();
        assert!(matches!(error, TaskManagerError::Conflict(_)));
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();

        manager.start_wda(wda_spec()).await.unwrap();
        let error = manager
            .start_ui_runner(ui_runner_spec(UiRunnerTarget::DeviceKit))
            .await
            .unwrap_err();
        assert!(matches!(error, TaskManagerError::Conflict(_)));
        manager.stop(TaskSelector::All).await.unwrap();

        let mut invalid = ui_runner_spec(UiRunnerTarget::DeviceKit);
        invalid.device_port = 8100;
        let error = manager.start_ui_runner(invalid).await.unwrap_err();
        assert!(matches!(
            error,
            TaskManagerError::Core(CoreError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn interrupted_xctest_is_failed_without_automatic_rerun() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let store = StateStore::new(&state_path);
        let record = new_record_with_policy(
            TaskSpec::XCTest(Box::new(xctest_spec())),
            RestartPolicy::Never,
        );
        let id = record.id;
        store
            .save(&PersistedState {
                schema_version: PersistedState::SCHEMA_VERSION,
                tasks: vec![record],
            })
            .unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let manager = manager(backend.clone(), &state_path).await;
        let restored = manager.show(id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Failed);
        assert_eq!(restored.desired_state, DesiredState::Stopped);
        assert!(restored.message.as_deref().unwrap().contains("interrupted"));
        assert_eq!(backend.xctest_count(), 0);
    }

    #[tokio::test]
    async fn interrupted_location_simulation_is_failed_without_automatic_rerun() {
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("state.json");
        let store = StateStore::new(&state_path);
        let record = new_record_with_policy(
            TaskSpec::LocationSimulation(location_simulation_spec(
                &directory.path().join("interrupted-route.gpx"),
            )),
            RestartPolicy::Never,
        );
        let id = record.id;
        store
            .save(&PersistedState {
                schema_version: PersistedState::SCHEMA_VERSION,
                tasks: vec![record],
            })
            .unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;

        let manager = manager(backend.clone(), &state_path).await;
        let restored = manager.show(id).await.unwrap();
        assert_eq!(restored.runtime_state, RuntimeState::Failed);
        assert_eq!(restored.desired_state, DesiredState::Stopped);
        assert!(restored.message.as_deref().unwrap().contains("interrupted"));
        assert_eq!(backend.location_session_count(), 0);
        assert_eq!(backend.location_clear_count(), 0);
    }

    #[tokio::test]
    async fn syslog_stream_output_is_readable_and_restored() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &state).await;
        let record = first
            .start_stream(stream_spec(TaskKind::Syslog, StreamFormat::JsonLines))
            .await
            .unwrap();
        wait_state(&first, record.id, RuntimeState::Running).await;
        let first_output = wait_output(&first, record.id, 20).await;
        let first_bytes = BASE64.decode(&first_output.bytes_base64).unwrap();
        assert!(
            String::from_utf8(first_bytes)
                .unwrap()
                .contains("mock syslog mock-phone")
        );
        let TaskSpec::Stream(spec) = &record.spec else {
            panic!("stream spec expected");
        };
        let output_path = PathBuf::from(spec.output_path.as_ref().unwrap());
        assert!(output_path.starts_with(directory.path().join("captures")));
        let original_length = first_output.total_bytes;
        first.shutdown().await;
        drop(first);

        let second = manager(backend, &state).await;
        wait_state(&second, record.id, RuntimeState::Running).await;
        let restored = wait_output(&second, record.id, original_length + 1).await;
        assert!(restored.total_bytes > original_length);
        assert_eq!(restored.content_type, "application/x-ndjson");
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn ostrace_filters_are_combined_and_reject_other_stream_kinds() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let options = OsTraceOptions {
            levels: vec![OsTraceLevel::Error, OsTraceLevel::Fault],
            subsystem_contains: Some("com.example.storage".into()),
            message_contains: Some("timeout".into()),
            message_excludes: Some("ignored".into()),
            follow: true,
        };
        let mut spec = stream_spec(TaskKind::OsTrace, StreamFormat::JsonLines);
        spec.os_trace = Some(options.clone());
        let record = manager.start_stream(spec).await.unwrap();
        wait_state(&manager, record.id, RuntimeState::Running).await;
        let output = wait_output_contains(&manager, record.id, "connection timeout").await;
        let text = String::from_utf8(BASE64.decode(&output.bytes_base64).unwrap()).unwrap();
        for line in text.lines() {
            let entry: Value = serde_json::from_str(line).unwrap();
            assert_eq!(entry["level"], "Error");
            assert_eq!(entry["label"]["subsystem"], "com.example.storage");
            assert!(entry["message"].as_str().unwrap().contains("timeout"));
            assert!(!entry["message"].as_str().unwrap().contains("ignored"));
        }
        let TaskSpec::Stream(saved) = &manager.show(record.id).await.unwrap().spec else {
            panic!("stream spec expected");
        };
        assert_eq!(saved.os_trace.as_ref(), Some(&options));

        let mut invalid = stream_spec(TaskKind::Syslog, StreamFormat::JsonLines);
        invalid.os_trace = Some(options);
        assert!(matches!(
            manager.start_stream(invalid).await,
            Err(TaskManagerError::Core(CoreError::InvalidArgument(_)))
        ));
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn accessibility_inspector_is_exclusive_readable_and_restored() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &state).await;
        let record = first
            .start_accessibility_inspector(accessibility_inspector_spec())
            .await
            .unwrap();
        wait_state(&first, record.id, RuntimeState::Running).await;
        let output =
            wait_output_contains(&first, record.id, "hostInspectorCurrentElementChanged:").await;
        assert_eq!(output.content_type, "application/x-ndjson");
        let text = String::from_utf8(BASE64.decode(&output.bytes_base64).unwrap()).unwrap();
        assert!(text.contains("inspector_started"));
        assert!(text.contains("hostInspectorCurrentElementChanged:"));
        let conflict = first
            .start_accessibility_inspector(accessibility_inspector_spec())
            .await
            .unwrap_err();
        assert!(matches!(conflict, TaskManagerError::Conflict(_)));
        let TaskSpec::AccessibilityInspector(spec) = &record.spec else {
            panic!("Accessibility inspector spec expected");
        };
        assert!(
            PathBuf::from(spec.output_path.as_ref().unwrap())
                .starts_with(directory.path().join("captures"))
        );
        let original_length = output.total_bytes;
        first.shutdown().await;
        drop(first);

        let second = manager(backend, &state).await;
        let restored = wait_state(&second, record.id, RuntimeState::Running).await;
        assert_eq!(restored.generation, 2);
        let output = wait_output(&second, record.id, original_length + 1).await;
        assert!(output.total_bytes > original_length);
        second.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_accessibility_inspector_start_keeps_one_owner() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let (first, second) = tokio::join!(
            manager.start_accessibility_inspector(accessibility_inspector_spec()),
            manager.start_accessibility_inspector(accessibility_inspector_spec()),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert!(matches!(
            first.err().or_else(|| second.err()),
            Some(TaskManagerError::Conflict(_))
        ));
        assert_eq!(
            manager
                .list(Some(TaskKind::AccessibilityInspector))
                .await
                .len(),
            1
        );
        manager.stop(TaskSelector::All).await.unwrap();
    }

    #[tokio::test]
    async fn pcap_stream_writes_one_header_and_binary_records() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let record = manager
            .start_stream(stream_spec(TaskKind::Pcap, StreamFormat::Pcap))
            .await
            .unwrap();
        wait_state(&manager, record.id, RuntimeState::Running).await;
        let output = wait_output(&manager, record.id, 25).await;
        let bytes = BASE64.decode(output.bytes_base64).unwrap();
        assert_eq!(&bytes[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(
            bytes
                .windows(4)
                .filter(|window| *window == [0xd4, 0xc3, 0xb2, 0xa1])
                .count(),
            1
        );
        manager.stop(TaskSelector::Id(record.id)).await.unwrap();
    }

    #[tokio::test]
    async fn remote_pairing_is_one_shot_and_tunnel_restores_from_server_home_identity() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let first = manager(backend.clone(), &state).await;

        let pairing = first
            .pair_remote_tunnel(TunnelPairSpec {
                udid: Some("mock-phone".into()),
                pair_record_path: "default".into(),
                manual: true,
            })
            .await
            .unwrap();
        let identity = PathBuf::from(&pairing.identity_path);
        assert_eq!(
            identity,
            directory.path().join("remote-pairing/selfIdentity.plist")
        );
        assert!(identity.is_file());
        assert!(StateStore::new(&state).load().unwrap().tasks.is_empty());

        let mut spec = tunnel_spec();
        spec.transport = TunnelTransport::RemotePairingTcp;
        spec.pair_record_path = "default".into();
        let task = first.start_tunnel(spec).await.unwrap();
        let running = wait_state(&first, task.id, RuntimeState::Running).await;
        let TaskSpec::Tunnel(spec) = &running.spec else {
            panic!("tunnel spec expected");
        };
        assert_eq!(spec.transport, TunnelTransport::RemotePairingTcp);
        assert_eq!(
            PathBuf::from(&spec.pair_record_path),
            identity.parent().unwrap()
        );

        first.shutdown().await;
        drop(first);
        let second = manager(backend, &state).await;
        let restored = wait_state(&second, task.id, RuntimeState::Running).await;
        assert_eq!(restored.generation, 2);
        second.stop(TaskSelector::Id(task.id)).await.unwrap();
    }

    #[tokio::test]
    async fn remote_pairing_tunnel_without_identity_fails_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let manager = manager(backend, &directory.path().join("state.json")).await;
        let mut spec = tunnel_spec();
        spec.transport = TunnelTransport::RemotePairingTcp;
        spec.pair_record_path = "default".into();
        let task = manager.start_tunnel(spec).await.unwrap();
        let failed = wait_state(&manager, task.id, RuntimeState::Failed).await;
        assert!(
            failed
                .message
                .as_deref()
                .unwrap()
                .contains("RemotePairing identity is missing")
        );
        manager.stop(TaskSelector::Id(task.id)).await.unwrap();
    }

    #[test]
    fn probe_failure_verdict_debounces_before_rebuild() {
        // 同步 go-ios fork 3172596:连续失败未达阈值保留隧道,达到阈值才重建。
        assert_eq!(probe_failure_verdict(0), ProbeFailureVerdict::KeepTunnel);
        assert_eq!(probe_failure_verdict(1), ProbeFailureVerdict::KeepTunnel);
        assert_eq!(
            probe_failure_verdict(PROBE_FAILURES_BEFORE_REBUILD - 1),
            ProbeFailureVerdict::KeepTunnel
        );
        assert_eq!(
            probe_failure_verdict(PROBE_FAILURES_BEFORE_REBUILD),
            ProbeFailureVerdict::Rebuild
        );
        assert_eq!(
            probe_failure_verdict(PROBE_FAILURES_BEFORE_REBUILD + 5),
            ProbeFailureVerdict::Rebuild
        );
    }
}
