use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use idevice::dvt::message::{Aux, AuxValue, Message, MessageHeader, PayloadHeader};
use idevice::provider::IdeviceProvider;
use idevice::{IdeviceService, ReadWrite};
use ios_device_core::accessibility_audit::{
    AccessibilityAuditClient, AccessibilityAuditHandshake, AuditEvent, FocusElement, MoveDirection,
};
use plist::{Dictionary, Value};
use rust_ios_protocol::{AccessibilityInspectorSpec, CommandInvocation};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject};
use rustls::{ClientConfig, DigitallySignedStruct};
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{CoreError, CoreResult, DeviceStream, DeviceStreamInfo, ServiceTransport};

const LEGACY_SERVICE_NAME: &str = "com.apple.accessibility.axAuditDaemon.remoteserver";
const AUDIT_COMPLETED_SELECTOR: &str = "hostDeviceDidCompleteAuditCategoriesWithAuditIssues:";
const INTEGER_AUDIT_TYPES_API_VERSION: u64 = 21;
const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
const INSPECTOR_STEP_TIMEOUT: Duration = Duration::from_secs(10);
const FOCUS_CHANGED_SELECTOR: &str = "hostInspectorCurrentElementChanged:";
const MONITORED_EVENT_CHANGED_SELECTOR: &str = "hostInspectorMonitoredEventTypeChanged:";

const DTX_MAGIC: u32 = 0x1f3d_5b79;

struct AccessibilityDtxClient {
    stream: Box<dyn ReadWrite>,
    next_identifier: u32,
}

struct AccessibilityDtxMessage {
    identifier: u32,
    conversation_index: u32,
    channel: i32,
    expects_reply: bool,
    aux: Option<Aux>,
    data: Option<Value>,
}

struct AccessibilityInspectorStream {
    client: AccessibilityAuditClient<Box<dyn ReadWrite>>,
    info: DeviceStreamInfo,
    queued: VecDeque<Vec<u8>>,
    initial_moves_remaining: u8,
    closed: bool,
}

impl fmt::Debug for AccessibilityInspectorStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessibilityInspectorStream")
            .field("info", &self.info)
            .field("queued_events", &self.queued.len())
            .field("initial_moves_remaining", &self.initial_moves_remaining)
            .field("closed", &self.closed)
            .finish()
    }
}

#[derive(Debug)]
struct NoServerNameVerification {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for NoServerNameVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _certificate: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _certificate: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

impl AccessibilityDtxClient {
    fn new(stream: Box<dyn ReadWrite>) -> Self {
        Self {
            stream,
            next_identifier: 1,
        }
    }

    async fn call_with_reply(
        &mut self,
        selector: &str,
        arguments: Option<Vec<AuxValue>>,
    ) -> CoreResult<AccessibilityDtxMessage> {
        let identifier = self.send_method(selector, arguments, true).await?;
        loop {
            let message = self.read_message().await?;
            if message.conversation_index == 0 {
                if message.expects_reply {
                    self.send_ack(&message).await?;
                }
                continue;
            }
            if message.identifier == identifier {
                tracing::debug!(selector, identifier, "received Accessibility DTX reply");
                return Ok(message);
            }
        }
    }

    async fn send_without_reply(
        &mut self,
        selector: &str,
        arguments: Option<Vec<AuxValue>>,
    ) -> CoreResult<()> {
        self.send_method(selector, arguments, false).await?;
        Ok(())
    }

    async fn next_incoming(&mut self, selector: &str) -> CoreResult<AccessibilityDtxMessage> {
        loop {
            let message = self.read_message().await?;
            if message.conversation_index != 0 {
                continue;
            }
            if message.expects_reply {
                self.send_ack(&message).await?;
            }
            if message_contains_string(&message, selector) {
                return Ok(message);
            }
        }
    }

    async fn send_method(
        &mut self,
        selector: &str,
        arguments: Option<Vec<AuxValue>>,
        expects_reply: bool,
    ) -> CoreResult<u32> {
        let identifier = self.next_identifier;
        self.next_identifier = self.next_identifier.wrapping_add(1).max(1);
        let message = Message::new(
            MessageHeader::new(0, 1, identifier, 0, 0, expects_reply),
            PayloadHeader::method_invocation(),
            arguments.map(Aux::from_values),
            Some(Value::String(selector.into())),
        );
        self.stream
            .write_all(&message.serialize())
            .await
            .map_err(map_io_error)?;
        self.stream.flush().await.map_err(map_io_error)?;
        Ok(identifier)
    }

    async fn send_ack(&mut self, message: &AccessibilityDtxMessage) -> CoreResult<()> {
        let conversation_index = message.conversation_index.saturating_add(1);
        let mut bytes = Vec::with_capacity(48);
        bytes.extend_from_slice(&DTX_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&message.identifier.to_le_bytes());
        bytes.extend_from_slice(&conversation_index.to_le_bytes());
        bytes.extend_from_slice(&message.channel.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&[0_u8; 16]);
        self.stream.write_all(&bytes).await.map_err(map_io_error)?;
        self.stream.flush().await.map_err(map_io_error)
    }

    async fn read_message(&mut self) -> CoreResult<AccessibilityDtxMessage> {
        let mut packet_data = Vec::new();
        let header = loop {
            let mut bytes = [0_u8; 32];
            self.stream
                .read_exact(&mut bytes)
                .await
                .map_err(map_io_error)?;
            let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
            let header_length = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            if magic != DTX_MAGIC || header_length != 32 {
                return Err(CoreError::DeviceUnavailable(format!(
                    "invalid Accessibility DTX header magic={magic:#x} length={header_length}"
                )));
            }
            let fragment_id = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
            let fragment_count = u16::from_le_bytes(bytes[10..12].try_into().unwrap());
            let length = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
            if length > 16 * 1024 * 1024 {
                return Err(CoreError::DeviceUnavailable(format!(
                    "Accessibility DTX frame is too large: {length} bytes"
                )));
            }
            let identifier = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            let conversation_index = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
            let wire_channel = i32::from_le_bytes(bytes[24..28].try_into().unwrap());
            let channel = if conversation_index.is_multiple_of(2) {
                -wire_channel
            } else {
                wire_channel
            };
            let expects_reply = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) == 1;
            if fragment_count > 1 && fragment_id == 0 {
                if length != 0 {
                    return Err(CoreError::DeviceUnavailable(format!(
                        "Accessibility DTX fragment header unexpectedly carries {length} bytes"
                    )));
                }
                continue;
            }
            let mut payload = vec![0_u8; length];
            self.stream
                .read_exact(&mut payload)
                .await
                .map_err(map_io_error)?;
            packet_data.extend_from_slice(&payload);
            if fragment_count == 0 || fragment_id == fragment_count - 1 {
                break (identifier, conversation_index, channel, expects_reply);
            }
        };

        if packet_data.len() < 16 {
            return Err(CoreError::DeviceUnavailable(format!(
                "Accessibility DTX payload header is truncated: {} bytes",
                packet_data.len()
            )));
        }
        let auxiliary_length = u32::from_le_bytes(packet_data[4..8].try_into().unwrap()) as usize;
        let total_length = u32::from_le_bytes(packet_data[8..12].try_into().unwrap()) as usize;
        if total_length < auxiliary_length || 16 + total_length > packet_data.len() {
            return Err(CoreError::DeviceUnavailable(format!(
                "invalid Accessibility DTX payload lengths aux={auxiliary_length} total={total_length} frame={}",
                packet_data.len()
            )));
        }
        let aux = if auxiliary_length == 0 {
            None
        } else {
            Some(
                Aux::from_bytes(packet_data[16..16 + auxiliary_length].to_vec())
                    .map_err(map_device_error)?,
            )
        };
        let data_start = 16 + auxiliary_length;
        let data_end = 16 + total_length;
        let data = if data_start == data_end {
            None
        } else {
            Some(
                ns_keyed_archive::decode::from_bytes(&packet_data[data_start..data_end]).map_err(
                    |error| {
                        CoreError::DeviceUnavailable(format!(
                            "decode Accessibility DTX payload: {error}"
                        ))
                    },
                )?,
            )
        };
        Ok(AccessibilityDtxMessage {
            identifier: header.0,
            conversation_index: header.1,
            channel: header.2,
            expects_reply: header.3,
            aux,
            data,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct AccessibilityRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct AccessibilityAuditIssue {
    issue_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    element_rect: Option<AccessibilityRect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform_element_value: Option<String>,
    #[serde(skip)]
    platform_element_bytes: Option<Vec<u8>>,
}

pub(crate) async fn start_inspector(
    provider: &dyn IdeviceProvider,
    spec: &AccessibilityInspectorSpec,
) -> CoreResult<Box<dyn DeviceStream>> {
    if let Some(font_size) = spec.font_size
        && (!font_size.is_finite() || font_size <= 0.0)
    {
        return Err(CoreError::InvalidArgument(
            "ax --font must be a finite number greater than zero".into(),
        ));
    }

    let (stream, product_major_version, service_transport) = connect_inspector(provider).await?;
    let mut client = AccessibilityAuditClient::new_with_handshake(
        stream,
        product_major_version,
        AccessibilityAuditHandshake::PublishCapabilities,
    );
    let setup = tokio::time::timeout(Duration::from_secs(45), async {
        let capabilities = client.capabilities().await.map_err(map_inspector_error)?;
        let api_version = client.api_version().await.map_err(map_inspector_error)?;
        let audit_types = client
            .supported_audit_types()
            .await
            .map_err(map_inspector_error)?;
        client
            .set_app_monitoring_enabled(true)
            .await
            .map_err(map_inspector_error)?;

        // Match go-ios SwitchToDevice before enabling selection mode.
        client
            .set_monitored_event_type(0)
            .await
            .map_err(map_inspector_error)?;
        client
            .wait_for_event(MONITORED_EVENT_CHANGED_SELECTOR)
            .await
            .map_err(map_inspector_error)?;
        client
            .focus_on_element()
            .await
            .map_err(map_inspector_error)?;
        client
            .wait_for_event(FOCUS_CHANGED_SELECTOR)
            .await
            .map_err(map_inspector_error)?;
        client
            .preview_on_element()
            .await
            .map_err(map_inspector_error)?;
        client
            .highlight_issue()
            .await
            .map_err(map_inspector_error)?;
        client
            .set_show_visuals(false)
            .await
            .map_err(map_inspector_error)?;

        let settings = client.settings().await.map_err(map_inspector_error)?;
        client
            .set_show_ignored_elements(false)
            .await
            .map_err(map_inspector_error)?;
        client
            .set_audit_target_pid(0)
            .await
            .map_err(map_inspector_error)?;
        client
            .focus_on_element()
            .await
            .map_err(map_inspector_error)?;
        client
            .wait_for_event(FOCUS_CHANGED_SELECTOR)
            .await
            .map_err(map_inspector_error)?;
        client
            .preview_on_element()
            .await
            .map_err(map_inspector_error)?;
        client
            .highlight_issue()
            .await
            .map_err(map_inspector_error)?;

        client
            .set_monitored_event_type(2)
            .await
            .map_err(map_inspector_error)?;
        client
            .set_show_visuals(true)
            .await
            .map_err(map_inspector_error)?;
        client
            .wait_for_event(MONITORED_EVENT_CHANGED_SELECTOR)
            .await
            .map_err(map_inspector_error)?;

        let font_update = if let Some(font_size) = spec.font_size {
            Some(
                client
                    .update_accessibility_setting("DYNAMIC_TYPE", font_size)
                    .await
                    .map_err(map_inspector_error)?,
            )
        } else {
            None
        };
        CoreResult::Ok(json!({
            "event": "inspector_started",
            "apiVersion": api_version,
            "capabilities": capabilities,
            "supportedAuditTypes": audit_types,
            "settings": settings,
            "fontSize": spec.font_size,
            "fontUpdate": font_update,
            "serviceTransport": service_transport,
        }))
    })
    .await
    .map_err(|_| {
        CoreError::Timeout(
            "Accessibility inspector setup did not complete within 45 seconds".into(),
        )
    })??;

    let mut queued = VecDeque::new();
    queued.push_back(json_line(&setup)?);
    Ok(Box::new(AccessibilityInspectorStream {
        client,
        info: DeviceStreamInfo {
            content_type: "application/x-ndjson".into(),
            initial_bytes: Vec::new(),
            service_transport,
        },
        queued,
        initial_moves_remaining: 3,
        closed: false,
    }))
}

#[async_trait::async_trait]
impl DeviceStream for AccessibilityInspectorStream {
    fn info(&self) -> &DeviceStreamInfo {
        &self.info
    }

    async fn next_chunk(&mut self) -> CoreResult<Vec<u8>> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(event);
        }
        if self.initial_moves_remaining > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let event = tokio::time::timeout(INSPECTOR_STEP_TIMEOUT, async {
                self.client
                    .move_focus(MoveDirection::Next)
                    .await
                    .map_err(map_inspector_error)?;
                self.client
                    .wait_for_event(FOCUS_CHANGED_SELECTOR)
                    .await
                    .map_err(map_inspector_error)
            })
            .await
            .map_err(|_| {
                CoreError::Timeout(
                    "Accessibility inspector focus did not change within 10 seconds".into(),
                )
            })??;
            self.initial_moves_remaining -= 1;
            return inspector_event_line("initial_focus", event);
        }

        let event = self
            .client
            .next_event()
            .await
            .map_err(map_inspector_error)?;
        inspector_event_line("notification", event)
    }

    async fn close(&mut self) -> CoreResult<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut cleanup_errors = Vec::new();
        match tokio::time::timeout(Duration::from_secs(1), async {
            self.client
                .set_monitored_event_type(0)
                .await
                .map_err(map_inspector_error)?;
            self.client
                .wait_for_event(MONITORED_EVENT_CHANGED_SELECTOR)
                .await
                .map_err(map_inspector_error)?;
            self.client
                .focus_on_element()
                .await
                .map_err(map_inspector_error)?;
            self.client
                .wait_for_event(FOCUS_CHANGED_SELECTOR)
                .await
                .map_err(map_inspector_error)?;
            self.client
                .preview_on_element()
                .await
                .map_err(map_inspector_error)?;
            self.client
                .highlight_issue()
                .await
                .map_err(map_inspector_error)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_errors.push(error.to_string()),
            Err(_) => cleanup_errors.push("turn-off handshake timed out".into()),
        }
        match tokio::time::timeout(
            Duration::from_millis(500),
            self.client.set_show_visuals(false),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_errors.push(format!("hide visuals: {error}")),
            Err(_) => cleanup_errors.push("hide visuals timed out".into()),
        }
        match tokio::time::timeout(
            Duration::from_millis(500),
            self.client.set_app_monitoring_enabled(false),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => cleanup_errors.push(format!("disable app monitoring: {error}")),
            Err(_) => cleanup_errors.push("disable app monitoring timed out".into()),
        }
        if cleanup_errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::DeviceUnavailable(format!(
                "Accessibility inspector cleanup failed: {}",
                cleanup_errors.join("; ")
            )))
        }
    }
}

async fn connect_inspector(
    provider: &dyn IdeviceProvider,
) -> CoreResult<(Box<dyn ReadWrite>, u64, ServiceTransport)> {
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(map_device_error)?;
    let mut lockdown = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let _legacy = lockdown
        .start_session(&pairing)
        .await
        .map_err(map_device_error)?;
    let product_version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await
        .map_err(map_device_error)?
        .as_string()
        .ok_or_else(|| CoreError::DeviceUnavailable("ProductVersion is not a string".into()))?
        .to_owned();
    let product_major_version = product_version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!("invalid ProductVersion {product_version:?}"))
        })?;
    let (port, enable_service_ssl) = lockdown
        .start_service(LEGACY_SERVICE_NAME)
        .await
        .map_err(map_device_error)?;
    let idevice = provider.connect(port).await.map_err(map_device_error)?;
    let mut stream = idevice
        .get_socket()
        .ok_or_else(|| CoreError::DeviceUnavailable("Accessibility socket is missing".into()))?;
    if enable_service_ssl {
        stream = tls_handshake_only(stream, &pairing).await?;
    }
    Ok((stream, product_major_version, ServiceTransport::Lockdown))
}

fn inspector_event_line(kind: &str, event: AuditEvent) -> CoreResult<Vec<u8>> {
    let focus = (event.selector == FOCUS_CHANGED_SELECTOR)
        .then(|| FocusElement::from_event_payload(&event.data).ok())
        .flatten();
    json_line(&json!({
        "event": kind,
        "selector": event.selector,
        "focus": focus,
        "data": event.data,
    }))
}

fn json_line(value: &JsonValue) -> CoreResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(|error| {
        CoreError::Backend(format!("serialize Accessibility inspector event: {error}"))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn map_inspector_error(
    error: ios_device_core::accessibility_audit::AccessibilityAuditError,
) -> CoreError {
    CoreError::DeviceUnavailable(format!("Accessibility inspector failed: {error}"))
}

pub(crate) async fn execute(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
) -> CoreResult<JsonValue> {
    let command = invocation.argv.first().map(String::as_str).unwrap_or("");
    let (action, options_start) = match command {
        "ax" => {
            let action = invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
                CoreError::InvalidArgument("ax requires the audit subcommand".into())
            })?;
            if action != "audit" {
                return Err(CoreError::NotImplemented(format!(
                    "ax action {action:?} is a persistent inspector workflow and is not implemented"
                )));
            }
            ("audit", 2)
        }
        "resetax" => ("reset", 1),
        _ => {
            return Err(CoreError::InvalidArgument(format!(
                "unexpected Accessibility command {command:?}"
            )));
        }
    };
    validate_options(&invocation.argv, options_start)?;
    let timeout_seconds = option_value(&invocation.argv, "--timeout")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                CoreError::InvalidArgument(format!(
                    "--timeout must be an integer number of seconds, got {value:?}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    if timeout_seconds == 0 || timeout_seconds > 600 {
        return Err(CoreError::InvalidArgument(
            "--timeout must be between 1 and 600 seconds".into(),
        ));
    }

    let (mut remote, service_transport) = connect(provider).await?;
    let timeout = Duration::from_secs(timeout_seconds);
    match action {
        "audit" => {
            let (api_version, audit_types, issues) =
                tokio::time::timeout(timeout, run_audit(&mut remote))
                    .await
                    .map_err(|_| {
                        CoreError::Timeout(format!(
                            "Accessibility audit did not complete within {timeout_seconds} seconds"
                        ))
                    })??;
            Ok(json!({
                "apiVersion": api_version,
                "auditTypes": audit_types,
                "issueCount": issues.len(),
                "issues": issues,
                "serviceTransport": service_transport,
            }))
        }
        "reset" => {
            tokio::time::timeout(timeout, reset_to_defaults(&mut remote))
                .await
                .map_err(|_| {
                    CoreError::Timeout(format!(
                        "resetax did not send within {timeout_seconds} seconds"
                    ))
                })??;
            Ok(json!({
                "status": "reset",
                "serviceTransport": service_transport,
            }))
        }
        _ => unreachable!(),
    }
}

async fn connect(
    provider: &dyn IdeviceProvider,
) -> CoreResult<(AccessibilityDtxClient, ServiceTransport)> {
    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(map_device_error)?;
    let mut lockdown = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let _legacy = lockdown
        .start_session(&pairing)
        .await
        .map_err(map_device_error)?;
    let (port, enable_service_ssl) = lockdown
        .start_service(LEGACY_SERVICE_NAME)
        .await
        .map_err(map_device_error)?;
    let idevice = provider.connect(port).await.map_err(map_device_error)?;
    let mut stream = idevice
        .get_socket()
        .ok_or_else(|| CoreError::DeviceUnavailable("Accessibility socket is missing".into()))?;
    if enable_service_ssl {
        stream = tls_handshake_only(stream, &pairing).await?;
    }
    Ok((
        AccessibilityDtxClient::new(stream),
        ServiceTransport::Lockdown,
    ))
}

async fn tls_handshake_only(
    stream: Box<dyn ReadWrite>,
    pairing: &idevice::pairing_file::PairingFile,
) -> CoreResult<Box<dyn ReadWrite>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut root_store = rustls::RootCertStore::empty();
    root_store
        .add(pairing.root_certificate.clone())
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "load Accessibility pairing root certificate: {error}"
            ))
        })?;
    let private_key =
        PrivateKeyDer::from_pem_slice(&pairing.host_private_key).map_err(|error| {
            CoreError::DeviceUnavailable(format!("load Accessibility pairing private key: {error}"))
        })?;
    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store.clone())
        .with_client_auth_cert(vec![pairing.host_certificate.clone()], private_key)
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!("configure Accessibility TLS: {error}"))
        })?;
    let inner = WebPkiServerVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!("configure Accessibility verifier: {error}"))
        })?;
    config
        .dangerous()
        .set_certificate_verifier(Arc::new(NoServerNameVerification { inner }));
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from("Device").map_err(|error| {
        CoreError::DeviceUnavailable(format!("invalid Accessibility TLS server name: {error}"))
    })?;
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!("Accessibility TLS handshake failed: {error}"))
        })?;
    let (stream, _) = tls_stream.into_inner();
    Ok(stream)
}

async fn run_audit(
    remote: &mut AccessibilityDtxClient,
) -> CoreResult<(u64, Vec<Value>, Vec<AccessibilityAuditIssue>)> {
    let api_reply = remote.call_with_reply("deviceApiVersion", None).await?;
    let api_version = message_values(&api_reply)
        .iter()
        .find_map(find_unsigned_integer)
        .ok_or_else(|| {
            CoreError::DeviceUnavailable("deviceApiVersion returned no integer API version".into())
        })?;

    let (types_selector, begin_selector) = audit_selectors(api_version);
    let types_reply = remote.call_with_reply(types_selector, None).await?;
    let audit_types = message_values(&types_reply)
        .iter()
        .find_map(find_scalar_array)
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!("{types_selector} returned no audit type array"))
        })?;
    if audit_types.is_empty() {
        return Err(CoreError::DeviceUnavailable(format!(
            "{types_selector} returned an empty audit type array"
        )));
    }

    remote
        .send_without_reply(
            begin_selector,
            Some(vec![AuxValue::archived_value(Value::Array(
                audit_types.clone(),
            ))]),
        )
        .await?;

    let completion = remote.next_incoming(AUDIT_COMPLETED_SELECTOR).await?;
    let mut issues = parse_audit_issues(&message_values(&completion));
    for issue in &mut issues {
        let Some(platform_element) = issue.platform_element_bytes.as_deref() else {
            continue;
        };
        if let Ok(Some(label)) = query_label(remote, platform_element).await
            && !label.is_empty()
        {
            issue.label = Some(label);
        }
    }
    Ok((api_version, audit_types, issues))
}

async fn reset_to_defaults(remote: &mut AccessibilityDtxClient) -> CoreResult<()> {
    remote
        .send_without_reply("deviceResetToDefaultAccessibilitySettings", None)
        .await
}

async fn query_label(
    remote: &mut AccessibilityDtxClient,
    platform_element: &[u8],
) -> CoreResult<Option<String>> {
    let mut element_value = Dictionary::new();
    element_value.insert(
        "PlatformElementValue_v1".into(),
        passthrough(Value::Data(platform_element.to_vec())),
    );
    let element = typed_object(
        "AXAuditElement_v1",
        passthrough(Value::Dictionary(element_value)),
    );

    let mut attribute_value = Dictionary::new();
    attribute_value.insert(
        "AttributeNameValue_v1".into(),
        passthrough(Value::String("Label".into())),
    );
    let attribute = typed_object(
        "AXAuditElementAttribute_v1",
        passthrough(Value::Dictionary(attribute_value)),
    );
    let reply = remote
        .call_with_reply(
            "deviceElement:valueForAttribute:",
            Some(vec![
                AuxValue::archived_value(element),
                AuxValue::archived_value(attribute),
            ]),
        )
        .await?;
    Ok(message_values(&reply).iter().find_map(find_response_string))
}

fn typed_object(object_type: &str, value: Value) -> Value {
    let mut dictionary = Dictionary::new();
    dictionary.insert("ObjectType".into(), Value::String(object_type.into()));
    dictionary.insert("Value".into(), value);
    Value::Dictionary(dictionary)
}

fn passthrough(value: Value) -> Value {
    typed_object("passthrough", value)
}

fn audit_selectors(api_version: u64) -> (&'static str, &'static str) {
    if api_version >= INTEGER_AUDIT_TYPES_API_VERSION {
        ("deviceAllSupportedAuditTypes", "deviceBeginAuditTypes:")
    } else {
        ("deviceAllAuditCaseIDs", "deviceBeginAuditCaseIDs:")
    }
}

fn message_values(message: &AccessibilityDtxMessage) -> Vec<Value> {
    let mut values = Vec::new();
    if let Some(value) = &message.data {
        values.push(value.clone());
    }
    if let Some(auxiliary) = &message.aux {
        for value in &auxiliary.values {
            match value {
                AuxValue::Array(bytes) => {
                    if let Ok(value) = ns_keyed_archive::decode::from_bytes(bytes) {
                        values.push(value);
                    }
                }
                AuxValue::String(value) => values.push(Value::String(value.clone())),
                AuxValue::U32(value) => values.push(Value::Integer((*value as u64).into())),
                AuxValue::I64(value) => values.push(Value::Integer((*value).into())),
                AuxValue::Double(value) => values.push(Value::Real(*value)),
                AuxValue::Null | AuxValue::PrimitiveDictionary(_) => {}
            }
        }
    }
    values
}

fn message_contains_string(message: &AccessibilityDtxMessage, expected: &str) -> bool {
    message_values(message)
        .iter()
        .any(|value| contains_string(value, expected))
}

fn contains_string(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected || value.trim_matches('"') == expected,
        Value::Array(values) => values.iter().any(|value| contains_string(value, expected)),
        Value::Dictionary(values) => values
            .values()
            .any(|value| contains_string(value, expected)),
        _ => false,
    }
}

fn find_unsigned_integer(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_unsigned_integer() {
        return Some(value);
    }
    match value {
        Value::Array(values) => values.iter().find_map(find_unsigned_integer),
        Value::Dictionary(values) => values.values().find_map(find_unsigned_integer),
        _ => None,
    }
}

fn find_scalar_array(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(values)
            if !values.is_empty()
                && values
                    .iter()
                    .all(|value| matches!(value, Value::String(_) | Value::Integer(_))) =>
        {
            Some(values.clone())
        }
        Value::Array(values) => values.iter().find_map(find_scalar_array),
        Value::Dictionary(values) => values.values().find_map(find_scalar_array),
        _ => None,
    }
}

fn parse_audit_issues(values: &[Value]) -> Vec<AccessibilityAuditIssue> {
    let mut issues = Vec::new();
    for value in values {
        collect_audit_issues(value, &mut issues);
    }
    issues
}

fn collect_audit_issues(value: &Value, output: &mut Vec<AccessibilityAuditIssue>) {
    match value {
        Value::Dictionary(dictionary) => {
            if has_issue_key(dictionary)
                && let Some(issue) = parse_issue_dictionary(dictionary)
            {
                output.push(issue);
                return;
            }
            for value in dictionary.values() {
                collect_audit_issues(value, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_audit_issues(value, output);
            }
        }
        _ => {}
    }
}

fn has_issue_key(dictionary: &Dictionary) -> bool {
    [
        "auditTestTypeValue_v1",
        "IssueClassificationValue_v1",
        "ElementRectValue_v1",
        "AuditElementValue_v1",
    ]
    .iter()
    .any(|key| dictionary.contains_key(key))
}

fn parse_issue_dictionary(dictionary: &Dictionary) -> Option<AccessibilityAuditIssue> {
    let issue_type = dictionary
        .get("auditTestTypeValue_v1")
        .and_then(find_response_string)
        .or_else(|| {
            dictionary
                .get("IssueClassificationValue_v1")
                .and_then(find_number)
                .map(|classification| audit_type_description(classification as i64))
        })
        .unwrap_or_default();
    let element_rect = dictionary.get("ElementRectValue_v1").and_then(extract_rect);
    let platform_element_bytes = dictionary
        .get("AuditElementValue_v1")
        .and_then(find_platform_element)
        .or_else(|| {
            dictionary
                .get("PlatformElementValue_v1")
                .and_then(find_data)
        });
    if issue_type.is_empty() && element_rect.is_none() {
        return None;
    }
    let platform_element_value = platform_element_bytes
        .as_ref()
        .map(|bytes| BASE64.encode(bytes));
    Some(AccessibilityAuditIssue {
        issue_type,
        label: None,
        element_rect,
        platform_element_value,
        platform_element_bytes,
    })
}

fn audit_type_description(classification: i64) -> String {
    match classification {
        12 | 13 => "testTypeContrast".into(),
        100 => "testTypeHitRegion".into(),
        1000 => "testTypeElementDetection".into(),
        3001 | 3002 => "testTypeDynamicText".into(),
        3003 => "testTypeTextClipped".into(),
        5000 => "testTypeSufficientElementDescription".into(),
        other => other.to_string(),
    }
}

fn find_response_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if value != "passthrough" => Some(value.clone()),
        Value::Array(values) => values.iter().find_map(find_response_string),
        Value::Dictionary(dictionary) => {
            if dictionary.contains_key("ObjectType")
                && let Some(value) = dictionary.get("Value")
            {
                return find_response_string(value);
            }
            dictionary
                .iter()
                .filter(|(key, _)| key.as_str() != "ObjectType")
                .find_map(|(_, value)| find_response_string(value))
        }
        _ => None,
    }
}

fn find_number(value: &Value) -> Option<f64> {
    if let Some(value) = value.as_real() {
        return Some(value);
    }
    if let Some(value) = value.as_signed_integer() {
        return Some(value as f64);
    }
    if let Some(value) = value.as_unsigned_integer() {
        return Some(value as f64);
    }
    match value {
        Value::Array(values) => values.iter().find_map(find_number),
        Value::Dictionary(values) => values.values().find_map(find_number),
        _ => None,
    }
}

fn find_platform_element(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Dictionary(dictionary) => {
            if let Some(value) = dictionary.get("PlatformElementValue_v1")
                && let Some(bytes) = find_data(value)
            {
                return Some(bytes);
            }
            dictionary.values().find_map(find_platform_element)
        }
        Value::Array(values) => values.iter().find_map(find_platform_element),
        _ => None,
    }
}

fn find_data(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Data(value) => Some(value.clone()),
        Value::Array(values) => values.iter().find_map(find_data),
        Value::Dictionary(values) => values.values().find_map(find_data),
        _ => None,
    }
}

fn extract_rect(value: &Value) -> Option<AccessibilityRect> {
    match value {
        Value::String(value) => parse_rect_string(value),
        Value::Array(values) if values.len() >= 4 => Some(AccessibilityRect {
            x: find_number(&values[0])?,
            y: find_number(&values[1])?,
            width: find_number(&values[2])?,
            height: find_number(&values[3])?,
        }),
        Value::Array(values) => values.iter().find_map(extract_rect),
        Value::Dictionary(dictionary) => {
            for keys in [
                ["x", "y", "width", "height"],
                ["X", "Y", "Width", "Height"],
                ["XValue", "YValue", "WidthValue", "HeightValue"],
            ] {
                if let (Some(x), Some(y), Some(width), Some(height)) = (
                    dictionary.get(keys[0]).and_then(find_number),
                    dictionary.get(keys[1]).and_then(find_number),
                    dictionary.get(keys[2]).and_then(find_number),
                    dictionary.get(keys[3]).and_then(find_number),
                ) {
                    return Some(AccessibilityRect {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
            dictionary.values().find_map(extract_rect)
        }
        _ => None,
    }
}

fn parse_rect_string(value: &str) -> Option<AccessibilityRect> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for character in value.chars().chain(std::iter::once(',')) {
        if character.is_ascii_digit() || matches!(character, '-' | '+' | '.' | 'e' | 'E') {
            current.push(character);
        } else if !current.is_empty() {
            if let Ok(value) = current.parse::<f64>() {
                numbers.push(value);
            }
            current.clear();
        }
    }
    (numbers.len() >= 4).then(|| AccessibilityRect {
        x: numbers[0],
        y: numbers[1],
        width: numbers[2],
        height: numbers[3],
    })
}

fn validate_options(arguments: &[String], start: usize) -> CoreResult<()> {
    let mut index = start;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--timeout" {
            if arguments.get(index + 1).is_none() {
                return Err(CoreError::InvalidArgument(
                    "--timeout requires a value".into(),
                ));
            }
            index += 2;
        } else if argument.starts_with("--timeout=") {
            index += 1;
        } else {
            return Err(CoreError::InvalidArgument(format!(
                "unexpected Accessibility argument {argument:?}"
            )));
        }
    }
    Ok(())
}

fn option_value(arguments: &[String], option: &str) -> Option<String> {
    for (index, argument) in arguments.iter().enumerate() {
        if argument == option {
            return arguments.get(index + 1).cloned();
        }
        if let Some(value) = argument.strip_prefix(&format!("{option}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

fn map_device_error(error: idevice::IdeviceError) -> CoreError {
    let message = error.to_string();
    if message.to_ascii_lowercase().contains("pair") {
        CoreError::PairingRequired(message)
    } else {
        CoreError::DeviceUnavailable(message)
    }
}

fn map_io_error(error: std::io::Error) -> CoreError {
    CoreError::DeviceUnavailable(format!("Accessibility DTX I/O failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(value: Value) -> Value {
        passthrough(value)
    }

    #[test]
    fn selectors_follow_ax_api_version() {
        assert_eq!(
            audit_selectors(20),
            ("deviceAllAuditCaseIDs", "deviceBeginAuditCaseIDs:")
        );
        assert_eq!(
            audit_selectors(21),
            ("deviceAllSupportedAuditTypes", "deviceBeginAuditTypes:")
        );
    }

    #[test]
    fn parses_classification_rect_and_platform_element() {
        let platform_element = vec![0x50, 0x67, 0x41, 0x01];
        let mut platform = Dictionary::new();
        platform.insert(
            "PlatformElementValue_v1".into(),
            wrapped(Value::Data(platform_element.clone())),
        );
        let mut issue = Dictionary::new();
        issue.insert(
            "IssueClassificationValue_v1".into(),
            wrapped(Value::Integer(12.into())),
        );
        issue.insert(
            "ElementRectValue_v1".into(),
            wrapped(Value::String("{{10, 20}, {30, 40}}".into())),
        );
        issue.insert(
            "AuditElementValue_v1".into(),
            wrapped(Value::Dictionary(platform)),
        );
        let parsed = parse_audit_issues(&[Value::Array(vec![Value::Dictionary(issue)])]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].issue_type, "testTypeContrast");
        assert_eq!(
            parsed[0].element_rect,
            Some(AccessibilityRect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            })
        );
        assert_eq!(
            parsed[0].platform_element_value.as_deref(),
            Some(BASE64.encode(platform_element).as_str())
        );
    }

    #[test]
    fn parses_device_provided_type_and_dictionary_rect() {
        let mut rect = Dictionary::new();
        rect.insert("x".into(), Value::Real(1.5));
        rect.insert("y".into(), Value::Integer(2.into()));
        rect.insert("width".into(), Value::Integer(100.into()));
        rect.insert("height".into(), Value::Integer(44.into()));
        let mut issue = Dictionary::new();
        issue.insert(
            "auditTestTypeValue_v1".into(),
            Value::String("testTypeHitRegion".into()),
        );
        issue.insert("ElementRectValue_v1".into(), Value::Dictionary(rect));
        let parsed = parse_audit_issues(&[Value::Dictionary(issue)]);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].issue_type, "testTypeHitRegion");
        assert_eq!(parsed[0].element_rect.as_ref().unwrap().x, 1.5);
    }

    #[tokio::test]
    async fn dtx_client_acks_incoming_callbacks_before_correlated_reply() {
        let (client_stream, mut device_stream) = tokio::io::duplex(64 * 1024);
        let device = tokio::spawn(async move {
            let request = Message::from_reader(&mut device_stream).await.unwrap();
            assert!(request.data.is_some());

            let callback = Message::new(
                MessageHeader::new(0, 1, 77, 0, 0, true),
                PayloadHeader::method_invocation(),
                None,
                Some(Value::String("hostApiVersion".into())),
            );
            device_stream
                .write_all(&callback.serialize())
                .await
                .unwrap();

            let mut ack = [0_u8; 48];
            device_stream.read_exact(&mut ack).await.unwrap();
            assert_eq!(u32::from_le_bytes(ack[0..4].try_into().unwrap()), DTX_MAGIC);
            assert_eq!(u32::from_le_bytes(ack[16..20].try_into().unwrap()), 77);
            assert_eq!(u32::from_le_bytes(ack[20..24].try_into().unwrap()), 1);
            assert_eq!(&ack[32..48], &[0_u8; 16]);

            let reply = Message::new(
                MessageHeader::new(0, 1, 1, 1, 0, false),
                PayloadHeader::method_invocation(),
                None,
                Some(Value::Integer(26.into())),
            );
            device_stream.write_all(&reply.serialize()).await.unwrap();
        });

        let mut client = AccessibilityDtxClient::new(Box::new(client_stream));
        let reply = client
            .call_with_reply("deviceApiVersion", None)
            .await
            .unwrap();
        assert_eq!(
            message_values(&reply)
                .iter()
                .find_map(find_unsigned_integer),
            Some(26)
        );
        device.await.unwrap();
    }
}
