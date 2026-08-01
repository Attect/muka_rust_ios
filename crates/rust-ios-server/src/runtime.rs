use std::fs::{File, OpenOptions};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use rust_ios_core::{DeviceBackend, DeviceRegistry};
use rust_ios_protocol::{
    ClientMessage, CommandResult, DeviceStateSpec, ErrorCode, PROTOCOL_MAJOR, PROTOCOL_MINOR,
    ProtocolError, Request, ResponseEnvelope, ResponsePayload, ServerHello, ServerMessage,
    ServerStatus, TaskSelector, read_json_frame, write_json_frame,
};
use serde_json::json;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    ServerConfig, ServerMetadata, StateStore, TaskManager, TaskManagerError,
    constant_time_token_eq, load_or_create_token,
};

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("server is already running for data directory {0}")]
    AlreadyRunning(PathBuf),
    #[error("server I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("server task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Task(#[from] TaskManagerError),
}

pub struct PreparedServer {
    listener: TcpListener,
    context: Arc<ServerContext>,
    _lock: File,
}

struct ServerContext {
    config: ServerConfig,
    listen_addr: SocketAddr,
    token: String,
    instance_id: Uuid,
    started_at_ms: u64,
    started_at: Instant,
    registry: Arc<DeviceRegistry>,
    tasks: Arc<TaskManager>,
    shutdown: CancellationToken,
}

impl std::fmt::Debug for ServerContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerContext")
            .field("listen_addr", &self.listen_addr)
            .field("instance_id", &self.instance_id)
            .field("started_at_ms", &self.started_at_ms)
            .field("backend", &self.registry.backend().name())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct InProcessServer {
    pub listen_addr: SocketAddr,
    pub token: String,
    pub instance_id: Uuid,
    shutdown: CancellationToken,
    join: JoinHandle<Result<(), ServerError>>,
}

impl InProcessServer {
    pub async fn shutdown(self) -> Result<(), ServerError> {
        self.shutdown.cancel();
        self.join.await?
    }
}

pub async fn prepare_server(
    config: ServerConfig,
    backend: Arc<dyn DeviceBackend>,
) -> Result<PreparedServer, ServerError> {
    config.paths.ensure()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&config.paths.lock)?;
    FileExt::try_lock_exclusive(&lock)
        .map_err(|_| ServerError::AlreadyRunning(config.paths.root.clone()))?;

    let token = load_or_create_token(&config.paths.token)?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    let listen_addr = listener.local_addr()?;
    let instance_id = Uuid::new_v4();
    let started_at_ms = unix_time_ms();
    let tasks =
        TaskManager::load(backend.clone(), StateStore::new(config.paths.state.clone())).await?;
    let registry = Arc::new(DeviceRegistry::new(backend));
    if let Err(error) = registry.refresh().await {
        warn!(%error, "initial device refresh failed; server will keep retrying");
    }

    let metadata = ServerMetadata {
        schema_version: ServerMetadata::SCHEMA_VERSION,
        pid: std::process::id(),
        instance_id,
        server_version: env!("CARGO_PKG_VERSION").into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        listen_addr,
        started_at_ms,
        executable: std::env::current_exe().unwrap_or_default(),
    };
    metadata.write_atomic(&config.paths.metadata)?;

    Ok(PreparedServer {
        listener,
        context: Arc::new(ServerContext {
            config,
            listen_addr,
            token,
            instance_id,
            started_at_ms,
            started_at: Instant::now(),
            registry,
            tasks,
            shutdown: CancellationToken::new(),
        }),
        _lock: lock,
    })
}

pub async fn run_server(
    config: ServerConfig,
    backend: Arc<dyn DeviceBackend>,
) -> Result<(), ServerError> {
    prepare_server(config, backend).await?.serve().await
}

/// CLI server 模式使用的 backend 工厂。测试可显式设置 `RUST_IOS_BACKEND=mock`，
/// 生产默认始终使用真实 `idevice` backend。
pub async fn run_default_server(config: ServerConfig) -> Result<(), ServerError> {
    let backend_name = std::env::var("RUST_IOS_BACKEND").unwrap_or_else(|_| "idevice".into());
    let backend: Arc<dyn DeviceBackend> = if backend_name.eq_ignore_ascii_case("mock") {
        let mock = rust_ios_core::MockDeviceBackend::new();
        if let Ok(devices) = std::env::var("RUST_IOS_MOCK_DEVICES") {
            for udid in devices
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                mock.attach_usb(udid.to_owned()).await;
            }
        }
        Arc::new(mock)
    } else if backend_name.eq_ignore_ascii_case("replay") {
        let fixture = std::env::var_os("RUST_IOS_REPLAY_FILE").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "RUST_IOS_REPLAY_FILE is required when RUST_IOS_BACKEND=replay",
            )
        })?;
        Arc::new(
            rust_ios_core::ReplayDeviceBackend::from_path(fixture)
                .await
                .map_err(io::Error::other)?,
        )
    } else {
        Arc::new(rust_ios_core::IdeviceBackend::default())
    };
    let backend: Arc<dyn DeviceBackend> = match std::env::var_os("RUST_IOS_RECORD_FILE") {
        Some(path) => Arc::new(rust_ios_core::RecordingBackend::new(backend, path)),
        None => backend,
    };
    run_server(config, backend).await
}

pub async fn spawn_in_process(
    config: ServerConfig,
    backend: Arc<dyn DeviceBackend>,
) -> Result<InProcessServer, ServerError> {
    let prepared = prepare_server(config, backend).await?;
    let listen_addr = prepared.context.listen_addr;
    let token = prepared.context.token.clone();
    let instance_id = prepared.context.instance_id;
    let shutdown = prepared.context.shutdown.clone();
    let join = tokio::spawn(prepared.serve());
    Ok(InProcessServer {
        listen_addr,
        token,
        instance_id,
        shutdown,
        join,
    })
}

impl PreparedServer {
    pub async fn serve(self) -> Result<(), ServerError> {
        let context = self.context.clone();
        let registry_cancel = context.shutdown.child_token();
        let registry = context.registry.clone();
        let poll_interval = context.config.device_poll_interval;
        let registry_task = tokio::spawn(async move {
            registry.run(poll_interval, registry_cancel).await;
        });

        info!(
            addr = %context.listen_addr,
            instance_id = %context.instance_id,
            backend = context.registry.backend().name(),
            "rust-ios server started"
        );

        loop {
            tokio::select! {
                () = context.shutdown.cancelled() => break,
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) if peer.ip().is_loopback() => {
                            let connection_context = context.clone();
                            tokio::spawn(async move {
                                if let Err(error) = handle_connection(connection_context, stream).await {
                                    debug!(%peer, %error, "client connection ended with error");
                                }
                            });
                        }
                        Ok((_stream, peer)) => warn!(%peer, "rejected non-loopback client"),
                        Err(error) => {
                            if context.shutdown.is_cancelled() {
                                break;
                            }
                            error!(%error, "failed to accept client");
                        }
                    }
                }
            }
        }

        context.shutdown.cancel();
        let _ = timeout(context.config.shutdown_timeout, registry_task).await;
        context.tasks.shutdown().await;
        cleanup_metadata(&context.config.paths.metadata, context.instance_id);
        info!(instance_id = %context.instance_id, "rust-ios server stopped");
        Ok(())
    }
}

async fn handle_connection(context: Arc<ServerContext>, mut stream: TcpStream) -> io::Result<()> {
    let hello = timeout(
        context.config.handshake_timeout,
        read_json_frame::<_, ClientMessage>(&mut stream),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client hello timed out"))?
    .map_err(io::Error::other)?;

    let ClientMessage::Hello(hello) = hello else {
        let error = ProtocolError::new(ErrorCode::InvalidRequest, "first frame must be hello");
        write_json_frame(&mut stream, &ServerMessage::Fatal(error))
            .await
            .map_err(io::Error::other)?;
        return Ok(());
    };

    if !constant_time_token_eq(&hello.auth_token, &context.token) {
        let error = ProtocolError::new(
            ErrorCode::AuthenticationFailed,
            "server authentication failed",
        );
        write_json_frame(&mut stream, &ServerMessage::Fatal(error))
            .await
            .map_err(io::Error::other)?;
        return Ok(());
    }
    if hello.protocol_major != PROTOCOL_MAJOR {
        let error = ProtocolError::new(
            ErrorCode::ProtocolMismatch,
            format!(
                "client protocol {}.{} is incompatible with server {}.{}",
                hello.protocol_major, hello.protocol_minor, PROTOCOL_MAJOR, PROTOCOL_MINOR
            ),
        );
        write_json_frame(&mut stream, &ServerMessage::Fatal(error))
            .await
            .map_err(io::Error::other)?;
        return Ok(());
    }

    let server_hello = ServerHello {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        server_version: env!("CARGO_PKG_VERSION").into(),
        server_pid: std::process::id(),
        instance_id: context.instance_id,
        capabilities: vec![
            "server_v1".into(),
            "device_list".into(),
            "pair_record".into(),
            "persistent_tasks".into(),
            "persistent_forward".into(),
            "persistent_tunnel".into(),
            "windows_kernel_tunnel".into(),
            "persistent_dproxy".into(),
            "remote_pairing_tcp".into(),
            "persistent_streams".into(),
            "ostrace_filters".into(),
            "persistent_accessibility_inspector".into(),
            "finite_location_simulation".into(),
            "persistent_ui_stream".into(),
            "persistent_ui_runner".into(),
            "persistent_device_state".into(),
            "persistent_webinspector_cdp".into(),
            "interactive_webinspector_js_shell".into(),
            "debug_app_session".into(),
            "wda_event_capture".into(),
            "xctest_unit_filters".into(),
            "shared_xctest_rsd".into(),
            "finite_xctest".into(),
            "finite_app_process".into(),
            "task_output".into(),
        ],
    };
    write_json_frame(&mut stream, &ServerMessage::Hello(server_hello))
        .await
        .map_err(io::Error::other)?;

    let mut client_owned_tasks = Vec::new();
    let connection_result = async {
        loop {
            let message = match read_json_frame::<_, ClientMessage>(&mut stream).await {
                Ok(message) => message,
                Err(rust_ios_protocol::FrameError::Io(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(io::Error::other(error)),
            };
            let ClientMessage::Request(envelope) = message else {
                let error = ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "hello may only be sent once per connection",
                );
                write_json_frame(&mut stream, &ServerMessage::Fatal(error))
                    .await
                    .map_err(io::Error::other)?;
                return Ok(());
            };

            let request_id = envelope.request_id;
            let starts_client_owned_task = matches!(
                &envelope.request,
                Request::DebugProxyStart { spec } if spec.client_owned
            );
            let stops_client_owned_task = match &envelope.request {
                Request::TaskStop {
                    selector: TaskSelector::Id(task_id),
                } => Some(*task_id),
                _ => None,
            };
            let (response, shutdown) = dispatch_request(&context, envelope.request).await;
            if starts_client_owned_task && let Ok(ResponsePayload::Task { task }) = &response {
                client_owned_tasks.push(task.id);
            }
            if response.is_ok()
                && let Some(task_id) = stops_client_owned_task
            {
                client_owned_tasks.retain(|value| *value != task_id);
            }
            let response = match response {
                Ok(payload) => ResponseEnvelope::ok(request_id, payload),
                Err(error) => ResponseEnvelope::error(request_id, error),
            };
            write_json_frame(&mut stream, &ServerMessage::Response(response))
                .await
                .map_err(io::Error::other)?;
            if shutdown {
                context.shutdown.cancel();
                return Ok(());
            }
        }
    }
    .await;
    for task_id in client_owned_tasks {
        if let Err(error) = context.tasks.stop(TaskSelector::Id(task_id)).await {
            debug!(%task_id, %error, "failed to release client-owned task after disconnect");
        }
    }
    connection_result
}

async fn dispatch_request(
    context: &ServerContext,
    request: Request,
) -> (Result<ResponsePayload, ProtocolError>, bool) {
    match request {
        Request::Ping => (
            Ok(ResponsePayload::Pong {
                server_time_ms: unix_time_ms(),
            }),
            false,
        ),
        Request::ServerStatus => {
            let devices = context.registry.snapshot().await;
            (
                Ok(ResponsePayload::ServerStatus(ServerStatus {
                    version: env!("CARGO_PKG_VERSION").into(),
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: PROTOCOL_MINOR,
                    pid: std::process::id(),
                    instance_id: context.instance_id,
                    started_at_ms: context.started_at_ms,
                    uptime_ms: context.started_at.elapsed().as_millis() as u64,
                    listen_addr: context.listen_addr.to_string(),
                    data_dir: context.config.paths.display_root(),
                    device_count: devices.devices.len(),
                    task_counts: context.tasks.counts().await,
                })),
                false,
            )
        }
        Request::Shutdown => (
            Ok(ResponsePayload::Ack {
                message: "server shutdown requested".into(),
            }),
            true,
        ),
        Request::ListDevices { .. } | Request::ListenSnapshot => {
            let result = context
                .registry
                .refresh()
                .await
                .map(ResponsePayload::Devices)
                .map_err(ProtocolError::from);
            (result, false)
        }
        Request::WaitDeviceChange {
            previous,
            timeout_ms,
        } => {
            if timeout_ms == 0 || timeout_ms > 30_000 {
                return (
                    Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        "device change timeout must be between 1 and 30000 ms",
                    )),
                    false,
                );
            }
            let mut receiver = context.registry.subscribe();
            let current = receiver.borrow_and_update().clone();
            if current != previous {
                (Ok(ResponsePayload::Devices(current)), false)
            } else {
                let _ = timeout(
                    Duration::from_millis(u64::from(timeout_ms)),
                    receiver.changed(),
                )
                .await;
                (
                    Ok(ResponsePayload::Devices(receiver.borrow().clone())),
                    false,
                )
            }
        }
        Request::ReadBuid => (
            context
                .registry
                .backend()
                .read_buid()
                .await
                .map(|buid| ResponsePayload::Buid { buid })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::ReadPairRecord { udid } => (
            context
                .registry
                .backend()
                .read_pair_record(&udid)
                .await
                .map(|record| ResponsePayload::PairRecord { udid, record })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TaskList { kind } => (
            Ok(ResponsePayload::Tasks {
                tasks: context.tasks.list(kind).await,
            }),
            false,
        ),
        Request::TaskShow { task_id } => (
            context
                .tasks
                .show(task_id)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TaskStop { selector } => (
            context
                .tasks
                .stop(selector)
                .await
                .map(|tasks| ResponsePayload::Tasks { tasks })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TaskRestart { task_id } => (
            context
                .tasks
                .restart(task_id)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TaskOutput {
            task_id,
            offset,
            limit,
        } => (
            context
                .tasks
                .read_output(task_id, offset, limit)
                .await
                .map(ResponsePayload::TaskOutput)
                .map_err(ProtocolError::from),
            false,
        ),
        Request::ForwardStart { specs } => (
            context
                .tasks
                .start_forward_many(specs)
                .await
                .map(|tasks| ResponsePayload::Tasks { tasks })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TunnelStart { spec } => (
            context
                .tasks
                .start_tunnel(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TunnelPair { spec } => (
            context
                .tasks
                .pair_remote_tunnel(spec)
                .await
                .map(|pairing| {
                    ResponsePayload::Command(CommandResult {
                        exit_code: 0,
                        value: Some(json!(pairing)),
                        text: None,
                        warnings: Vec::new(),
                    })
                })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::TunnelRefresh { udid } => (
            context
                .tasks
                .refresh_tunnels(udid)
                .await
                .map(|tasks| ResponsePayload::Tasks { tasks })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::StreamStart { spec } => (
            context
                .tasks
                .start_stream(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::AccessibilityInspectorStart { spec } => (
            context
                .tasks
                .start_accessibility_inspector(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::LocationSimulationStart { spec } => (
            context
                .tasks
                .start_location_simulation(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::ScreenshotStreamStart { spec } => (
            context
                .tasks
                .start_screenshot_stream(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::UiStreamStart { spec } => (
            context
                .tasks
                .start_ui_stream(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::UiRunnerStart { spec } => (
            context
                .tasks
                .start_ui_runner(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::WebInspectorCdpStart { spec } => (
            context
                .tasks
                .start_webinspector_cdp(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::WebInspectorJsShellStart { spec } => (
            context
                .tasks
                .start_webinspector_js_shell(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::WebInspectorJsShellEval {
            task_id,
            expression,
        } => (
            context
                .tasks
                .webinspector_js_shell_eval(task_id, expression)
                .await
                .map(ResponsePayload::Command)
                .map_err(ProtocolError::from),
            false,
        ),
        Request::DebugProxyStart { spec } => (
            context
                .tasks
                .start_debug_proxy(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::DproxyStart { spec } => (
            context
                .tasks
                .start_dproxy(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::WdaStart { spec } => (
            context
                .tasks
                .start_wda(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::XCTestStart { spec } => (
            context
                .tasks
                .start_xctest(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::AppLaunchStart { spec } => (
            context
                .tasks
                .start_app_launch(spec)
                .await
                .map(|task| ResponsePayload::Task { task })
                .map_err(ProtocolError::from),
            false,
        ),
        Request::Execute { invocation } => {
            if invocation.argv.first().map(String::as_str) == Some("devicestate")
                && invocation.argv.get(1).map(String::as_str) == Some("enable")
            {
                let condition_identifier = invocation
                    .argv
                    .get(2)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'));
                let profile_identifier = invocation
                    .argv
                    .get(3)
                    .filter(|value| !value.is_empty() && !value.starts_with('-'));
                let result = match (condition_identifier, profile_identifier) {
                    (Some(condition_identifier), Some(profile_identifier)) => context
                        .tasks
                        .start_device_state(DeviceStateSpec {
                            udid: invocation.udid,
                            condition_identifier: condition_identifier.clone(),
                            profile_identifier: profile_identifier.clone(),
                        })
                        .await
                        .map(|task| {
                            ResponsePayload::Command(CommandResult {
                                exit_code: 0,
                                value: Some(json!(task)),
                                text: None,
                                warnings: vec![
                                    "devicestate is server-owned; stop it with task stop".into(),
                                ],
                            })
                        })
                        .map_err(ProtocolError::from),
                    _ => Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        "devicestate enable requires profileTypeId and profileId",
                    )),
                };
                (result, false)
            } else {
                (
                    context
                        .tasks
                        .execute(&invocation)
                        .await
                        .map(ResponsePayload::Command)
                        .map_err(ProtocolError::from),
                    false,
                )
            }
        }
    }
}

fn cleanup_metadata(path: &std::path::Path, instance_id: Uuid) {
    match ServerMetadata::read(path) {
        Ok(metadata) if metadata.instance_id == instance_id => {
            if let Err(error) = std::fs::remove_file(path) {
                warn!(path = %path.display(), %error, "failed to remove server metadata");
            }
        }
        Ok(_) | Err(_) => {}
    }
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use rust_ios_core::MockDeviceBackend;
    use rust_ios_protocol::{
        ClientHello, DebugProxySpec, DproxyMode, DproxySpec, RequestEnvelope, ResponseBody,
        RuntimeState, TaskKind, UiRunnerSpec, UiRunnerTarget, WebInspectorCdpSpec,
        WebInspectorJsShellSpec,
    };

    use super::*;

    async fn test_server() -> (tempfile::TempDir, InProcessServer, MockDeviceBackend) {
        let directory = tempfile::tempdir().unwrap();
        let backend = MockDeviceBackend::new();
        let config = ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap());
        let server = spawn_in_process(config, Arc::new(backend.clone()))
            .await
            .unwrap();
        (directory, server, backend)
    }

    #[tokio::test]
    async fn ping_and_device_list_round_trip() {
        let (_directory, server, backend) = test_server().await;
        backend.attach_usb("mock-phone").await;
        let mut stream = TcpStream::connect(server.listen_addr).await.unwrap();
        write_json_frame(
            &mut stream,
            &ClientMessage::Hello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: "test".into(),
                client_pid: 1,
                auth_token: server.token.clone(),
            }),
        )
        .await
        .unwrap();
        let ServerMessage::Hello(hello) = read_json_frame::<_, ServerMessage>(&mut stream)
            .await
            .unwrap()
        else {
            panic!("expected server hello");
        };
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "ostrace_filters")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "debug_app_session")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "wda_event_capture")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "xctest_unit_filters")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "shared_xctest_rsd")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "windows_kernel_tunnel")
        );
        assert!(
            hello
                .capabilities
                .iter()
                .any(|value| value == "persistent_dproxy")
        );

        let request = RequestEnvelope::new(Request::ListDevices { details: true });
        let id = request.request_id;
        write_json_frame(&mut stream, &ClientMessage::Request(request))
            .await
            .unwrap();
        let ServerMessage::Response(response) = read_json_frame::<_, ServerMessage>(&mut stream)
            .await
            .unwrap()
        else {
            panic!("expected response");
        };
        assert_eq!(response.request_id, id);
        let ResponseBody::Ok(ResponsePayload::Devices(devices)) = response.response else {
            panic!("expected device list");
        };
        assert_eq!(devices.devices[0].udid, "mock-phone");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn webinspector_cdp_request_starts_server_owned_task() {
        let (_directory, server, backend) = test_server().await;
        backend.attach_usb("mock-phone").await;
        let mut client =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let ResponsePayload::Task { task } = client
            .request(Request::WebInspectorCdpStart {
                spec: WebInspectorCdpSpec {
                    udid: Some("mock-phone".into()),
                    bind_host: "127.0.0.1".into(),
                    port: 0,
                    startup_timeout_ms: 15_000,
                },
            })
            .await
            .unwrap()
        else {
            panic!("CDP task response expected");
        };
        assert_eq!(task.kind, rust_ios_protocol::TaskKind::WebInspectorCdp);
        let ResponsePayload::Tasks { tasks } = client
            .request(Request::TaskStop {
                selector: TaskSelector::Id(task.id),
            })
            .await
            .unwrap()
        else {
            panic!("task stop response expected");
        };
        assert_eq!(tasks.len(), 1);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn dproxy_request_starts_server_owned_task() {
        let (directory, server, _backend) = test_server().await;
        let mut client =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let ResponsePayload::Task { task } = client
            .request(Request::DproxyStart {
                spec: DproxySpec {
                    bind_host: "127.0.0.1".into(),
                    port: 0,
                    upstream_address: "127.0.0.1:27015".into(),
                    binary_only: false,
                    mode: DproxyMode::Usbmuxd,
                    interface: None,
                    output_path: Some(
                        directory
                            .path()
                            .join("dproxy-events.ndjson")
                            .to_string_lossy()
                            .into_owned(),
                    ),
                },
            })
            .await
            .unwrap()
        else {
            panic!("dproxy task response expected");
        };
        assert_eq!(task.kind, TaskKind::Dproxy);
        assert_eq!(task.runtime_state, RuntimeState::Running);
        client
            .request(Request::TaskStop {
                selector: TaskSelector::Id(task.id),
            })
            .await
            .unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn webinspector_js_shell_round_trip_uses_server_owned_input() {
        let (_directory, server, backend) = test_server().await;
        backend.attach_usb("mock-phone").await;
        let mut client =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let ResponsePayload::Task { task } = client
            .request(Request::WebInspectorJsShellStart {
                spec: WebInspectorJsShellSpec {
                    udid: Some("mock-phone".into()),
                    url: None,
                    bundle_id: None,
                    open_safari: false,
                    command_timeout_ms: 15_000,
                    output_path: None,
                },
            })
            .await
            .unwrap()
        else {
            panic!("JS shell task response expected");
        };
        assert_eq!(task.kind, rust_ios_protocol::TaskKind::WebInspectorJsShell);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let ResponsePayload::Task { task: current } = client
                .request(Request::TaskShow { task_id: task.id })
                .await
                .unwrap()
            else {
                panic!("task response expected");
            };
            if current.runtime_state == rust_ios_protocol::RuntimeState::Running {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "{current:#?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let ResponsePayload::Command(result) = client
            .request(Request::WebInspectorJsShellEval {
                task_id: task.id,
                expression: "2 + 2".into(),
            })
            .await
            .unwrap()
        else {
            panic!("JS shell eval response expected");
        };
        assert_eq!(result.value, Some(json!(4)));
        client
            .request(Request::TaskStop {
                selector: TaskSelector::Id(task.id),
            })
            .await
            .unwrap();
        assert_eq!(backend.webinspector_js_shell_close_count(), 1);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ui_runner_request_starts_server_owned_task() {
        let (_directory, server, backend) = test_server().await;
        backend.attach_usb("mock-phone").await;
        let mut client =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let ResponsePayload::Task { task } = client
            .request(Request::UiRunnerStart {
                spec: UiRunnerSpec {
                    target: UiRunnerTarget::Wda,
                    udid: Some("mock-phone".into()),
                    runner_bundle_id: "com.example.WDA.xctrunner".into(),
                    target_bundle_id: "com.example.WDA.xctrunner".into(),
                    xctest_config: "WebDriverAgentRunner.xctest".into(),
                    bind_host: "127.0.0.1".into(),
                    host_port: 0,
                    device_port: 8100,
                    health_path: "/status".into(),
                    readiness_timeout_ms: 3_000,
                    output_path: None,
                },
            })
            .await
            .unwrap()
        else {
            panic!("UI runner task response expected");
        };
        assert_eq!(task.kind, rust_ios_protocol::TaskKind::UiRunner);
        assert_eq!(task.runtime_state, rust_ios_protocol::RuntimeState::Running);
        client
            .request(Request::TaskStop {
                selector: TaskSelector::Id(task.id),
            })
            .await
            .unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn client_owned_debug_proxy_is_not_persisted_and_stops_on_disconnect() {
        let (directory, server, backend) = test_server().await;
        backend.attach_usb("mock-phone").await;
        let mut owner =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let ResponsePayload::Task { task } = owner
            .request(Request::DebugProxyStart {
                spec: DebugProxySpec {
                    udid: Some("mock-phone".into()),
                    bind_host: "127.0.0.1".into(),
                    port: 0,
                    client_owned: true,
                },
            })
            .await
            .unwrap()
        else {
            panic!("debug proxy task response expected");
        };
        let state = std::fs::read_to_string(directory.path().join("state-v1.json")).unwrap();
        assert!(!state.contains(&task.id.to_string()));

        drop(owner);
        let mut observer =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let ResponsePayload::Tasks { tasks } = observer
                .request(Request::TaskList {
                    kind: Some(TaskKind::DebugProxy),
                })
                .await
                .unwrap()
            else {
                panic!("debug proxy task list expected");
            };
            if tasks.is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_token() {
        let (_directory, server, _backend) = test_server().await;
        let mut stream = TcpStream::connect(server.listen_addr).await.unwrap();
        write_json_frame(
            &mut stream,
            &ClientMessage::Hello(ClientHello {
                protocol_major: PROTOCOL_MAJOR,
                protocol_minor: PROTOCOL_MINOR,
                client_version: "test".into(),
                client_pid: 1,
                auth_token: "0".repeat(64),
            }),
        )
        .await
        .unwrap();
        let ServerMessage::Fatal(error) = read_json_frame::<_, ServerMessage>(&mut stream)
            .await
            .unwrap()
        else {
            panic!("expected fatal error");
        };
        assert_eq!(error.code, ErrorCode::AuthenticationFailed);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn device_change_wait_is_driven_by_server_registry() {
        let (_directory, server, backend) = test_server().await;
        let mut client =
            crate::IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let wait = tokio::spawn(async move {
            client
                .request(Request::WaitDeviceChange {
                    previous: rust_ios_protocol::DeviceList::default(),
                    timeout_ms: 2_000,
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        backend.attach_usb("attached-later").await;
        let ResponsePayload::Devices(devices) = wait.await.unwrap().unwrap() else {
            panic!("expected changed device list");
        };
        assert_eq!(devices.devices[0].udid, "attached-later");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn lock_rejects_second_server() {
        let directory = tempfile::tempdir().unwrap();
        let backend = Arc::new(MockDeviceBackend::new());
        let first = spawn_in_process(
            ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap()),
            backend.clone(),
        )
        .await
        .unwrap();
        let second = prepare_server(
            ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap()),
            backend,
        )
        .await;
        assert!(matches!(second, Err(ServerError::AlreadyRunning(_))));
        first.shutdown().await.unwrap();
    }
}
