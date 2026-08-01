use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::task::{JoinError, JoinSet};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{BoxedIo, CoreError, CoreResult};

const USBMUXD_HEADER_LEN: usize = 16;
const MAX_USBMUXD_FRAME_LEN: usize = 16 * 1024 * 1024;
const DIRECTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DproxyCaptureSpec {
    pub connection_id: String,
    pub directory: PathBuf,
    pub binary_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DproxyConnectionStats {
    pub host_to_usbmuxd_bytes: u64,
    pub usbmuxd_to_host_bytes: u64,
    pub capture_directory: String,
}

#[derive(Debug, Default)]
struct DproxyCounters {
    host_to_usbmuxd: AtomicU64,
    usbmuxd_to_host: AtomicU64,
}

#[derive(Debug, Default)]
struct DproxyProtocolState {
    connect_tags: StdMutex<HashSet<u32>>,
    raw_mode: AtomicBool,
}

impl DproxyProtocolState {
    fn observe_plist(
        &self,
        direction: DproxyDirection,
        tag: u32,
        message_type: Option<&str>,
        plist: Option<&Value>,
    ) {
        match (direction, message_type) {
            (DproxyDirection::HostToUsbmuxd, Some("Connect")) => {
                self.connect_tags.lock().unwrap().insert(tag);
            }
            (DproxyDirection::UsbmuxdToHost, _)
                if plist.is_some_and(|value| value.get("Number").is_some()) =>
            {
                let mut tags = self.connect_tags.lock().unwrap();
                if tags.remove(&tag)
                    && plist
                        .and_then(|value| value.get("Number"))
                        .and_then(Value::as_u64)
                        == Some(0)
                {
                    self.raw_mode.store(true, Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }

    fn enter_raw_mode(&self) {
        self.raw_mode.store(true, Ordering::SeqCst);
    }

    fn is_raw_mode(&self) -> bool {
        self.raw_mode.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy)]
enum DproxyDirection {
    HostToUsbmuxd,
    UsbmuxdToHost,
}

impl DproxyDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::HostToUsbmuxd => "host_to_usbmuxd",
            Self::UsbmuxdToHost => "usbmuxd_to_host",
        }
    }

    fn raw_file(self) -> &'static str {
        match self {
            Self::HostToUsbmuxd => "host-to-usbmuxd.bin",
            Self::UsbmuxdToHost => "usbmuxd-to-host.bin",
        }
    }

    fn event_file(self) -> &'static str {
        match self {
            Self::HostToUsbmuxd => "host-to-usbmuxd.ndjson",
            Self::UsbmuxdToHost => "usbmuxd-to-host.ndjson",
        }
    }
}

/// 连接显式 usbmuxd upstream，并透明代理一个 host client，同时写入每方向 raw dump。
/// 普通模式还会尽力解析初始 usbmuxd plist frame；进入 device service raw stream 后停止解析。
pub async fn run_dproxy_connection(
    client: BoxedIo,
    upstream_address: SocketAddr,
    capture: DproxyCaptureSpec,
    cancel: CancellationToken,
) -> CoreResult<DproxyConnectionStats> {
    tokio::fs::create_dir_all(&capture.directory).await?;
    let upstream = timeout(Duration::from_secs(5), TcpStream::connect(upstream_address))
        .await
        .map_err(|_| CoreError::Timeout(format!("connect dproxy upstream {upstream_address}")))?
        .map_err(|error| {
            CoreError::UsbmuxdUnavailable(format!(
                "connect dproxy upstream {upstream_address}: {error}"
            ))
        })?;
    let upstream: BoxedIo = Box::new(upstream);
    let (client_reader, client_writer) = tokio::io::split(client);
    let (upstream_reader, upstream_writer) = tokio::io::split(upstream);
    let counters = Arc::new(DproxyCounters::default());
    let protocol = Arc::new(DproxyProtocolState::default());
    let connection_cancel = CancellationToken::new();
    let mut directions = JoinSet::new();

    directions.spawn(proxy_direction(
        client_reader,
        upstream_writer,
        capture.clone(),
        DproxyDirection::HostToUsbmuxd,
        counters.clone(),
        protocol.clone(),
        connection_cancel.clone(),
    ));
    directions.spawn(proxy_direction(
        upstream_reader,
        client_writer,
        capture.clone(),
        DproxyDirection::UsbmuxdToHost,
        counters.clone(),
        protocol,
        connection_cancel.clone(),
    ));

    let mut first_error = None;
    let mut shutting_down = false;
    while !directions.is_empty() {
        let result = if shutting_down {
            match timeout(DIRECTION_SHUTDOWN_TIMEOUT, directions.join_next()).await {
                Ok(result) => result,
                Err(_) => {
                    directions.abort_all();
                    while directions.join_next().await.is_some() {}
                    if first_error.is_none() && !cancel.is_cancelled() {
                        first_error = Some(CoreError::Timeout(
                            "stop dproxy connection directions".into(),
                        ));
                    }
                    break;
                }
            }
        } else {
            tokio::select! {
                () = cancel.cancelled() => {
                    connection_cancel.cancel();
                    shutting_down = true;
                    continue;
                }
                result = directions.join_next() => result,
            }
        };
        let error = join_direction_error(result);
        if first_error.is_none() && error.is_some() {
            first_error = error;
            connection_cancel.cancel();
            shutting_down = true;
        }
    }

    if let Some(error) = first_error
        && !cancel.is_cancelled()
    {
        return Err(error);
    }
    Ok(DproxyConnectionStats {
        host_to_usbmuxd_bytes: counters.host_to_usbmuxd.load(Ordering::SeqCst),
        usbmuxd_to_host_bytes: counters.usbmuxd_to_host.load(Ordering::SeqCst),
        capture_directory: capture.directory.to_string_lossy().into_owned(),
    })
}

fn join_direction_error(result: Option<Result<CoreResult<()>, JoinError>>) -> Option<CoreError> {
    match result {
        None | Some(Ok(Ok(()))) => None,
        Some(Ok(Err(error))) => Some(error),
        Some(Err(error)) if error.is_cancelled() => None,
        Some(Err(error)) => Some(CoreError::Backend(format!(
            "dproxy direction task failed: {error}"
        ))),
    }
}

async fn proxy_direction(
    mut source: ReadHalf<BoxedIo>,
    mut destination: WriteHalf<BoxedIo>,
    capture: DproxyCaptureSpec,
    direction: DproxyDirection,
    counters: Arc<DproxyCounters>,
    protocol: Arc<DproxyProtocolState>,
    cancel: CancellationToken,
) -> CoreResult<()> {
    let mut raw = create_capture_file(capture.directory.join(direction.raw_file())).await?;
    let mut events = if capture.binary_only {
        None
    } else {
        Some(create_capture_file(capture.directory.join(direction.event_file())).await?)
    };
    let mut decoder = UsbmuxdCaptureDecoder::default();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let length = tokio::select! {
            () = cancel.cancelled() => break,
            result = source.read(&mut buffer) => result?,
        };
        if length == 0 {
            break;
        }
        raw.write_all(&buffer[..length]).await?;
        if let Some(events) = events.as_mut() {
            for mut event in decoder.feed(&buffer[..length], direction, &protocol) {
                if let Some(object) = event.as_object_mut() {
                    object.insert(
                        "connectionId".into(),
                        Value::String(capture.connection_id.clone()),
                    );
                }
                let mut line = serde_json::to_vec(&event).map_err(|error| {
                    CoreError::Backend(format!("serialize dproxy capture event: {error}"))
                })?;
                line.push(b'\n');
                events.write_all(&line).await?;
            }
        }
        destination.write_all(&buffer[..length]).await?;
        match direction {
            DproxyDirection::HostToUsbmuxd => {
                counters
                    .host_to_usbmuxd
                    .fetch_add(length as u64, Ordering::SeqCst);
            }
            DproxyDirection::UsbmuxdToHost => {
                counters
                    .usbmuxd_to_host
                    .fetch_add(length as u64, Ordering::SeqCst);
            }
        }
    }
    raw.flush().await?;
    if let Some(events) = events.as_mut() {
        events.flush().await?;
    }
    let _ = destination.shutdown().await;
    Ok(())
}

async fn create_capture_file(path: PathBuf) -> CoreResult<File> {
    Ok(OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await?)
}

#[derive(Debug, Default)]
struct UsbmuxdCaptureDecoder {
    buffer: Vec<u8>,
    offset: u64,
    raw_mode: bool,
}

impl UsbmuxdCaptureDecoder {
    fn feed(
        &mut self,
        bytes: &[u8],
        direction: DproxyDirection,
        protocol: &DproxyProtocolState,
    ) -> Vec<Value> {
        if self.raw_mode {
            return Vec::new();
        }
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if protocol.is_raw_mode() {
                events.push(self.fallback_event(direction, "usbmuxd Connect succeeded"));
                break;
            }
            if self.buffer.len() < USBMUXD_HEADER_LEN {
                if self.buffer.len() > MAX_USBMUXD_FRAME_LEN {
                    protocol.enter_raw_mode();
                    events.push(self.fallback_event(direction, "buffer exceeded frame limit"));
                }
                break;
            }
            let length = u32::from_le_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
            let version = u32::from_le_bytes(self.buffer[4..8].try_into().unwrap());
            let message_type = u32::from_le_bytes(self.buffer[8..12].try_into().unwrap());
            let tag = u32::from_le_bytes(self.buffer[12..16].try_into().unwrap());
            if !(USBMUXD_HEADER_LEN..=MAX_USBMUXD_FRAME_LEN).contains(&length)
                || version > 2
                || message_type > 16
            {
                protocol.enter_raw_mode();
                events.push(
                    self.fallback_event(direction, "stream no longer contains usbmuxd frames"),
                );
                break;
            }
            if self.buffer.len() < length {
                break;
            }
            let payload = &self.buffer[USBMUXD_HEADER_LEN..length];
            let mut plist_value = plist::from_bytes::<plist::Value>(payload)
                .ok()
                .and_then(|value| serde_json::to_value(value).ok());
            if let Some(value) = plist_value.as_mut() {
                redact_capture_value(value);
            }
            let plist_message_type = plist_value
                .as_ref()
                .and_then(|value| value.get("MessageType"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            protocol.observe_plist(
                direction,
                tag,
                plist_message_type.as_deref(),
                plist_value.as_ref(),
            );
            events.push(json!({
                "timestampMs": unix_time_ms(),
                "direction": direction.as_str(),
                "offset": self.offset,
                "length": length,
                "version": version,
                "messageType": message_type,
                "tag": tag,
                "plistMessageType": plist_message_type,
                "plist": plist_value,
            }));
            self.buffer.drain(..length);
            self.offset = self.offset.saturating_add(length as u64);
        }
        events
    }

    fn fallback_event(&mut self, direction: DproxyDirection, reason: &str) -> Value {
        let buffered = self.buffer.len();
        self.buffer.clear();
        self.raw_mode = true;
        json!({
            "timestampMs": unix_time_ms(),
            "direction": direction.as_str(),
            "offset": self.offset,
            "event": "raw_stream",
            "reason": reason,
            "bufferedBytes": buffered,
        })
    }
}

fn redact_capture_value(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase();
                if normalized.contains("privatekey")
                    || normalized.contains("password")
                    || matches!(
                        normalized.as_str(),
                        "pairrecorddata" | "escrowbag" | "rootcertificate"
                    )
                {
                    *value = Value::String("<redacted>".into());
                } else {
                    redact_capture_value(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_capture_value),
        _ => {}
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn usbmuxd_packet(message_type: &str, private: bool) -> Vec<u8> {
        let mut dictionary = plist::Dictionary::from_iter([
            (
                "MessageType".to_owned(),
                plist::Value::String(message_type.to_owned()),
            ),
            (
                "ClientVersionString".to_owned(),
                plist::Value::String("rust-ios-test".into()),
            ),
        ]);
        if private {
            dictionary.insert(
                "PairRecordData".into(),
                plist::Value::Data(vec![1, 2, 3, 4]),
            );
        }
        let mut payload = Vec::new();
        plist::to_writer_xml(&mut payload, &plist::Value::Dictionary(dictionary)).unwrap();
        let mut packet = Vec::with_capacity(USBMUXD_HEADER_LEN + payload.len());
        packet.extend_from_slice(&((USBMUXD_HEADER_LEN + payload.len()) as u32).to_le_bytes());
        packet.extend_from_slice(&1_u32.to_le_bytes());
        packet.extend_from_slice(&8_u32.to_le_bytes());
        packet.extend_from_slice(&7_u32.to_le_bytes());
        packet.extend_from_slice(&payload);
        packet
    }

    #[test]
    fn decoder_handles_fragmented_frames_redacts_pairing_data_and_falls_back() {
        let packet = usbmuxd_packet("ReadPairRecord", true);
        let mut decoder = UsbmuxdCaptureDecoder::default();
        let protocol = DproxyProtocolState::default();
        assert!(
            decoder
                .feed(&packet[..9], DproxyDirection::HostToUsbmuxd, &protocol)
                .is_empty()
        );
        let events = decoder.feed(&packet[9..], DproxyDirection::HostToUsbmuxd, &protocol);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["plistMessageType"], "ReadPairRecord");
        assert_eq!(events[0]["plist"]["PairRecordData"], "<redacted>");

        let fallback = decoder.feed(&[0xff; 16], DproxyDirection::HostToUsbmuxd, &protocol);
        assert_eq!(fallback[0]["event"], "raw_stream");
        assert!(
            decoder
                .feed(&[0xff; 32], DproxyDirection::HostToUsbmuxd, &protocol)
                .is_empty()
        );
    }

    #[test]
    fn decoder_enters_raw_mode_after_successful_connect_result() {
        let protocol = DproxyProtocolState::default();
        let mut request_decoder = UsbmuxdCaptureDecoder::default();
        let mut response_decoder = UsbmuxdCaptureDecoder::default();
        let request = usbmuxd_packet("Connect", false);
        let request_events =
            request_decoder.feed(&request, DproxyDirection::HostToUsbmuxd, &protocol);
        assert_eq!(request_events[0]["plistMessageType"], "Connect");

        let mut dictionary =
            plist::Dictionary::from_iter([("Number".to_owned(), plist::Value::Integer(0.into()))]);
        dictionary.insert(
            "ClientVersionString".into(),
            plist::Value::String("rust-ios-test".into()),
        );
        let mut payload = Vec::new();
        plist::to_writer_xml(&mut payload, &plist::Value::Dictionary(dictionary)).unwrap();
        let mut response = Vec::new();
        response.extend_from_slice(&((USBMUXD_HEADER_LEN + payload.len()) as u32).to_le_bytes());
        response.extend_from_slice(&1_u32.to_le_bytes());
        response.extend_from_slice(&8_u32.to_le_bytes());
        response.extend_from_slice(&7_u32.to_le_bytes());
        response.extend_from_slice(&payload);
        let response_events =
            response_decoder.feed(&response, DproxyDirection::UsbmuxdToHost, &protocol);
        assert_eq!(response_events[0]["plistMessageType"], Value::Null);

        let raw_event = request_decoder.feed(
            b"device-service-request",
            DproxyDirection::HostToUsbmuxd,
            &protocol,
        );
        assert_eq!(raw_event[0]["event"], "raw_stream");
        assert_eq!(raw_event[0]["reason"], "usbmuxd Connect succeeded");
        assert!(
            request_decoder
                .feed(b"more", DproxyDirection::HostToUsbmuxd, &protocol)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn connection_proxies_echo_and_captures_both_directions() {
        let directory = tempfile::tempdir().unwrap();
        let capture_directory = directory.path().join("connection-1");
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let length = stream.read(&mut buffer).await.unwrap();
                if length == 0 {
                    break;
                }
                stream.write_all(&buffer[..length]).await.unwrap();
            }
        });
        let (mut caller, proxy_client) = tokio::io::duplex(128 * 1024);
        let capture = DproxyCaptureSpec {
            connection_id: "connection-1".into(),
            directory: capture_directory.clone(),
            binary_only: false,
        };
        let proxy_task = tokio::spawn(run_dproxy_connection(
            Box::new(proxy_client),
            upstream_address,
            capture,
            CancellationToken::new(),
        ));
        let packet = usbmuxd_packet("ListDevices", false);
        caller.write_all(&packet[..7]).await.unwrap();
        caller.write_all(&packet[7..]).await.unwrap();
        let mut echoed = vec![0_u8; packet.len()];
        caller.read_exact(&mut echoed).await.unwrap();
        assert_eq!(echoed, packet);
        caller.shutdown().await.unwrap();

        let stats = timeout(Duration::from_secs(3), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        upstream_task.await.unwrap();
        assert_eq!(stats.host_to_usbmuxd_bytes, packet.len() as u64);
        assert_eq!(stats.usbmuxd_to_host_bytes, packet.len() as u64);
        assert_eq!(
            tokio::fs::read(capture_directory.join("host-to-usbmuxd.bin"))
                .await
                .unwrap(),
            packet
        );
        assert_eq!(
            tokio::fs::read(capture_directory.join("usbmuxd-to-host.bin"))
                .await
                .unwrap(),
            packet
        );
        let events = tokio::fs::read_to_string(capture_directory.join("host-to-usbmuxd.ndjson"))
            .await
            .unwrap();
        assert!(events.contains("ListDevices"));
    }

    #[tokio::test]
    async fn connection_preserves_half_close_until_delayed_response_arrives() {
        let directory = tempfile::tempdir().unwrap();
        let capture_directory = directory.path().join("connection-half-close");
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let response = usbmuxd_packet("Result", false);
        let expected_response = response.clone();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
            request
        });
        let (mut caller, proxy_client) = tokio::io::duplex(128 * 1024);
        let proxy_task = tokio::spawn(run_dproxy_connection(
            Box::new(proxy_client),
            upstream_address,
            DproxyCaptureSpec {
                connection_id: "connection-half-close".into(),
                directory: capture_directory,
                binary_only: false,
            },
            CancellationToken::new(),
        ));
        let request = usbmuxd_packet("ListDevices", false);
        caller.write_all(&request).await.unwrap();
        caller.shutdown().await.unwrap();
        let mut received = vec![0_u8; expected_response.len()];
        timeout(Duration::from_secs(3), caller.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, expected_response);
        assert_eq!(upstream_task.await.unwrap(), request);
        let stats = timeout(Duration::from_secs(3), proxy_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(stats.host_to_usbmuxd_bytes, request.len() as u64);
        assert_eq!(stats.usbmuxd_to_host_bytes, expected_response.len() as u64);
    }
}
