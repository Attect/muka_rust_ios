use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use rust_ios_core::{CoreError, WebInspectorCdpSession, WebInspectorPageInfo};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::debug;

const CDP_PROTOCOL_VERSION: &str = "1.3";
const DEVICE_POLL_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Clone)]
struct CdpState {
    session: Arc<Mutex<Box<dyn WebInspectorCdpSession>>>,
    pages: Arc<RwLock<Vec<WebInspectorPageInfo>>>,
    websocket: Arc<Mutex<()>>,
    advertised_endpoint: String,
    fatal: mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
}

type HttpError = (StatusCode, Json<Value>);

pub(crate) async fn serve(
    listener: TcpListener,
    session: Box<dyn WebInspectorCdpSession>,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
    let endpoint = listener.local_addr()?;
    let info = session.info().clone();
    let advertised_endpoint = advertised_endpoint(endpoint);
    let session = Arc::new(Mutex::new(session));
    let (fatal, mut fatal_rx) = mpsc::unbounded_channel();
    let server_cancel = CancellationToken::new();
    let state = CdpState {
        session: session.clone(),
        pages: Arc::new(RwLock::new(info.pages)),
        websocket: Arc::new(Mutex::new(())),
        advertised_endpoint,
        fatal,
        cancel: server_cancel.clone(),
    };
    let router = Router::new()
        .route("/json", get(list_targets))
        .route("/json/list", get(list_targets))
        .route("/json/version", get(version))
        .route("/devtools/page/{page_key}", get(websocket))
        .with_state(state);
    let graceful_cancel = server_cancel.clone();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move { graceful_cancel.cancelled().await });
    let server_future = async move { server.await };
    tokio::pin!(server_future);

    let (result, wait_for_server) = tokio::select! {
        () = cancel.cancelled() => (Ok(()), true),
        fatal = fatal_rx.recv() => (Err(CoreError::DeviceUnavailable(
            fatal.unwrap_or_else(|| "WebInspector CDP session stopped unexpectedly".into())
        )), true),
        result = &mut server_future => (result.map_err(CoreError::Io), false),
    };
    server_cancel.cancel();
    if wait_for_server {
        let _ = (&mut server_future).await;
    }
    let close_result = session.lock().await.close().await;
    result.and(close_result)
}

async fn version() -> Json<Value> {
    Json(json!({
        "Browser": format!("rust-ios/{}", env!("CARGO_PKG_VERSION")),
        "Protocol-Version": CDP_PROTOCOL_VERSION,
    }))
}

async fn list_targets(State(state): State<CdpState>) -> Result<Json<Value>, HttpError> {
    let pages = refresh_pages(&state).await?;
    Ok(Json(Value::Array(
        pages
            .iter()
            .map(|page| target_json(page, &state.advertised_endpoint))
            .collect(),
    )))
}

async fn refresh_pages(state: &CdpState) -> Result<Vec<WebInspectorPageInfo>, HttpError> {
    let Ok(_websocket) = state.websocket.clone().try_lock_owned() else {
        return Ok(state.pages.read().await.clone());
    };
    let mut session = state.session.lock().await;
    match session.refresh_pages().await {
        Ok(pages) => {
            *state.pages.write().await = pages.clone();
            Ok(pages)
        }
        Err(error) => {
            let message = error.to_string();
            let _ = state.fatal.send(message.clone());
            Err(http_error(StatusCode::SERVICE_UNAVAILABLE, message))
        }
    }
}

async fn websocket(
    State(state): State<CdpState>,
    Path(page_key): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    if !state
        .pages
        .read()
        .await
        .iter()
        .any(|page| page.key == page_key)
    {
        return Err(http_error(
            StatusCode::NOT_FOUND,
            format!("no inspectable WebInspector page with key {page_key:?} was found"),
        ));
    }
    let active = state.websocket.clone().try_lock_owned().map_err(|_| {
        http_error(
            StatusCode::CONFLICT,
            "another CDP page WebSocket is already active",
        )
    })?;
    Ok(upgrade
        .on_upgrade(move |socket| bridge_websocket(socket, state, page_key, active))
        .into_response())
}

async fn bridge_websocket(
    socket: WebSocket,
    state: CdpState,
    page_key: String,
    _active: OwnedMutexGuard<()>,
) {
    let (mut output, mut input) = socket.split();
    let (browser_sender, mut browser_receiver) = mpsc::unbounded_channel();
    let reader_cancel = state.cancel.child_token();
    let child_cancel = reader_cancel.clone();
    let reader = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = child_cancel.cancelled() => return,
                incoming = input.next() => match incoming {
                    Some(Ok(message)) => {
                        if browser_sender.send(Ok(message)).is_err() {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = browser_sender.send(Err(error.to_string()));
                        return;
                    }
                    None => return,
                }
            }
        }
    });

    let result = bridge_page_session(&mut output, &state, &page_key, &mut browser_receiver).await;
    reader_cancel.cancel();
    let _ = reader.await;
    if let Err((message, fatal)) = result {
        if fatal {
            let _ = state.fatal.send(message.clone());
        }
        close_with_error(&mut output, &message).await;
    }
}

async fn bridge_page_session(
    output: &mut SplitSink<WebSocket, Message>,
    state: &CdpState,
    page_key: &str,
    browser: &mut mpsc::UnboundedReceiver<Result<Message, String>>,
) -> Result<(), (String, bool)> {
    let mut session = state.session.lock().await;
    if let Err(error) = session.attach_page(page_key).await {
        return Err((error.to_string(), false));
    }

    loop {
        if state.cancel.is_cancelled() {
            let _ = output.send(Message::Close(None)).await;
            return Ok(());
        }
        loop {
            let incoming = match browser.try_recv() {
                Ok(incoming) => incoming,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            };
            match incoming {
                Ok(Message::Text(text)) => {
                    let message = match serde_json::from_str::<Value>(text.as_str()) {
                        Ok(message) => message,
                        Err(error) => {
                            let response = json!({
                                "error": {
                                    "code": -32700,
                                    "message": format!("invalid CDP JSON: {error}"),
                                }
                            });
                            if send_json(output, response).await.is_err() {
                                return Ok(());
                            }
                            continue;
                        }
                    };
                    if let Err(error) = session.send_message(message).await {
                        return Err((error.to_string(), true));
                    }
                }
                Ok(Message::Ping(payload)) => {
                    if output.send(Message::Pong(payload)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Binary(_)) => {
                    return Err(("CDP WebSocket only accepts JSON text frames".into(), false));
                }
                Ok(Message::Close(_)) => return Ok(()),
                Err(error) => {
                    debug!(%error, "CDP browser WebSocket ended with error");
                    return Ok(());
                }
            }
        }
        match session.next_message(DEVICE_POLL_TIMEOUT).await {
            Ok(Some(message)) => {
                if send_json(output, message).await.is_err() {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) => {
                return Err((error.to_string(), true));
            }
        }
    }
}

async fn send_json(
    output: &mut SplitSink<WebSocket, Message>,
    value: Value,
) -> Result<(), axum::Error> {
    output.send(Message::Text(value.to_string().into())).await
}

async fn close_with_error(output: &mut SplitSink<WebSocket, Message>, message: &str) {
    let reason: String = message.chars().take(120).collect();
    let _ = output
        .send(Message::Close(Some(CloseFrame {
            code: close_code::ERROR,
            reason: reason.into(),
        })))
        .await;
}

fn target_json(page: &WebInspectorPageInfo, endpoint: &str) -> Value {
    let encoded_key = utf8_percent_encode(&page.key, NON_ALPHANUMERIC);
    json!({
        "description": page.bundle_id,
        "devtoolsFrontendUrl": format!(
            "devtools://devtools/bundled/inspector.html?ws={endpoint}/devtools/page/{encoded_key}"
        ),
        "id": page.key,
        "title": page.title,
        "type": "page",
        "url": page.url,
        "webSocketDebuggerUrl": format!("ws://{endpoint}/devtools/page/{encoded_key}"),
    })
}

fn advertised_endpoint(endpoint: std::net::SocketAddr) -> String {
    let host = if endpoint.ip().is_unspecified() {
        "127.0.0.1".into()
    } else if endpoint.is_ipv6() {
        format!("[{}]", endpoint.ip())
    } else {
        endpoint.ip().to_string()
    };
    format!("{host}:{}", endpoint.port())
}

fn http_error(status: StatusCode, message: impl Into<String>) -> HttpError {
    (status, Json(json!({ "error": message.into() })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_uses_encoded_page_key_and_chrome_fields() {
        let target = target_json(
            &WebInspectorPageInfo {
                key: "page key/1".into(),
                page_id: 7,
                application_id: "PID:42".into(),
                bundle_id: "com.apple.mobilesafari".into(),
                title: "Page".into(),
                url: "https://example.com/".into(),
            },
            "127.0.0.1:9222",
        );
        assert_eq!(target["type"], "page");
        assert_eq!(target["id"], "page key/1");
        assert!(
            target["webSocketDebuggerUrl"]
                .as_str()
                .unwrap()
                .contains("page%20key%2F1")
        );
    }
}
