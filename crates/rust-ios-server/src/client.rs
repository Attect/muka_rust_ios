use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use rust_ios_protocol::{
    ClientHello, ClientMessage, FrameError, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolError, Request,
    RequestEnvelope, ResponseBody, ResponsePayload, ServerHello, ServerMessage, read_json_frame,
    write_json_frame,
};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::platform::spawn_detached;
use crate::{ServerConfig, load_or_create_token, read_token};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("server connection timed out")]
    Timeout,
    #[error("server I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("server frame error: {0}")]
    Frame(#[from] FrameError),
    #[error("server rejected request: {0:?}")]
    Protocol(ProtocolError),
    #[error("unexpected server message: {0}")]
    Unexpected(String),
    #[error("failed to start server: {0}")]
    Spawn(String),
}

impl ClientError {
    pub fn server_unavailable(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::TimedOut
            ),
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct IpcClient {
    stream: TcpStream,
    pub hello: ServerHello,
}

impl IpcClient {
    pub async fn connect(
        addr: SocketAddr,
        token: &str,
        connect_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let mut stream = timeout(connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| ClientError::Timeout)??;
        let hello = ClientHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_pid: std::process::id(),
            auth_token: token.into(),
        };
        write_json_frame(&mut stream, &ClientMessage::Hello(hello)).await?;
        let response = timeout(connect_timeout, read_json_frame(&mut stream))
            .await
            .map_err(|_| ClientError::Timeout)??;
        match response {
            ServerMessage::Hello(hello) => {
                if hello.protocol_major != PROTOCOL_MAJOR {
                    return Err(ClientError::Unexpected(format!(
                        "server protocol {}.{} does not match client {}.{}",
                        hello.protocol_major, hello.protocol_minor, PROTOCOL_MAJOR, PROTOCOL_MINOR
                    )));
                }
                Ok(Self { stream, hello })
            }
            ServerMessage::Fatal(error) => Err(ClientError::Protocol(error)),
            other => Err(ClientError::Unexpected(format!(
                "expected server hello, received {other:?}"
            ))),
        }
    }

    pub async fn request(&mut self, request: Request) -> Result<ResponsePayload, ClientError> {
        let envelope = RequestEnvelope::new(request);
        let request_id = envelope.request_id;
        write_json_frame(&mut self.stream, &ClientMessage::Request(envelope)).await?;
        loop {
            match read_json_frame::<_, ServerMessage>(&mut self.stream).await? {
                ServerMessage::Response(response) => {
                    if response.request_id != request_id {
                        return Err(ClientError::Unexpected(format!(
                            "response id {} does not match request id {request_id}",
                            response.request_id
                        )));
                    }
                    return match response.response {
                        ResponseBody::Ok(payload) => Ok(payload),
                        ResponseBody::Error(error) => Err(ClientError::Protocol(error)),
                    };
                }
                ServerMessage::Event(_) => continue,
                ServerMessage::Fatal(error) => return Err(ClientError::Protocol(error)),
                ServerMessage::Hello(_) => {
                    return Err(ClientError::Unexpected("server sent a second hello".into()));
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct EnsuredServer {
    pub client: IpcClient,
    pub started: bool,
}

pub async fn connect_existing(config: &ServerConfig) -> Result<IpcClient, ClientError> {
    let token = read_token(&config.paths.token)?;
    IpcClient::connect(config.listen_addr, &token, config.handshake_timeout).await
}

pub async fn ensure_server(config: &ServerConfig) -> Result<EnsuredServer, ClientError> {
    let token = load_or_create_token(&config.paths.token)?;
    match IpcClient::connect(config.listen_addr, &token, config.handshake_timeout).await {
        Ok(client) => Ok(EnsuredServer {
            client,
            started: false,
        }),
        Err(error) if error.server_unavailable() => {
            let executable = std::env::current_exe()?;
            start_server_process(config, &executable, &token).await
        }
        Err(error) => Err(error),
    }
}

pub async fn start_server_process(
    config: &ServerConfig,
    executable: &Path,
    token: &str,
) -> Result<EnsuredServer, ClientError> {
    config.paths.ensure()?;
    if let Ok(client) =
        IpcClient::connect(config.listen_addr, token, Duration::from_millis(250)).await
    {
        return Ok(EnsuredServer {
            client,
            started: false,
        });
    }

    let arguments = vec![
        OsString::from("server"),
        OsString::from("nodaemon"),
        OsString::from("--daemon-child"),
        OsString::from("--server-addr"),
        OsString::from(config.listen_addr.to_string()),
        OsString::from("--server-home"),
        config.paths.root.as_os_str().to_owned(),
    ];
    let mut child = spawn_detached(executable, &arguments).map_err(|error| {
        ClientError::Spawn(format!("cannot spawn {}: {error}", executable.display()))
    })?;

    let child_pid = child.pid;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match IpcClient::connect(config.listen_addr, token, Duration::from_millis(300)).await {
            Ok(client) => {
                return Ok(EnsuredServer {
                    client,
                    started: true,
                });
            }
            Err(error) if error.server_unavailable() && Instant::now() < deadline => {
                if let Some(status) = child.try_wait()? {
                    return Err(ClientError::Spawn(format!(
                        "server process {child_pid} exited with code {status}; see {}",
                        config.paths.log.display()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
            }
            Err(error) if error.server_unavailable() => {
                return Err(ClientError::Spawn(format!(
                    "server process {child_pid} did not become ready before timeout; see {}",
                    config.paths.log.display()
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

pub async fn request_existing(
    config: &ServerConfig,
    request: Request,
) -> Result<ResponsePayload, ClientError> {
    connect_existing(config).await?.request(request).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_ios_core::MockDeviceBackend;
    use rust_ios_protocol::{Request, ResponsePayload};

    use crate::{ServerConfig, spawn_in_process};

    use super::*;

    #[tokio::test]
    async fn client_can_reuse_connection_for_multiple_requests() {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap());
        let server = spawn_in_process(config.clone(), Arc::new(MockDeviceBackend::new()))
            .await
            .unwrap();
        let mut client =
            IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        assert!(matches!(
            client.request(Request::Ping).await.unwrap(),
            ResponsePayload::Pong { .. }
        ));
        assert!(matches!(
            client.request(Request::ServerStatus).await.unwrap(),
            ResponsePayload::ServerStatus(_)
        ));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn forward_request_returns_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap());
        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let server = spawn_in_process(config, Arc::new(backend)).await.unwrap();
        let mut client =
            IpcClient::connect(server.listen_addr, &server.token, Duration::from_secs(1))
                .await
                .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            client.request(Request::ForwardStart {
                specs: vec![rust_ios_protocol::ForwardSpec {
                    udid: Some("mock-phone".into()),
                    bind_host: "127.0.0.1".into(),
                    host_port: 0,
                    device_port: 8100,
                    connection: rust_ios_protocol::ConnectionPreference::UsbPreferred,
                }],
            }),
        )
        .await
        .expect("forward request timed out")
        .unwrap();
        assert!(matches!(response, ResponsePayload::Tasks { .. }));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_is_a_protocol_error_not_an_autostart_signal() {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig::isolated(directory.path(), "127.0.0.1:0".parse().unwrap());
        let server = spawn_in_process(config, Arc::new(MockDeviceBackend::new()))
            .await
            .unwrap();
        let error = IpcClient::connect(server.listen_addr, &"f".repeat(64), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Protocol(_)));
        assert!(!error.server_unavailable());
        server.shutdown().await.unwrap();
    }
}
