use std::borrow::Cow;
use std::time::Duration;

use idevice::{Idevice, IdeviceError, IdeviceService};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use plist::{Dictionary, Value};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, USER_AGENT};

use crate::{CoreError, CoreResult};

pub(crate) const DRM_HANDSHAKE_URL: &str = "https://albert.apple.com/deviceservices/drmHandshake";
pub(crate) const DEVICE_ACTIVATION_URL: &str =
    "https://albert.apple.com/deviceservices/deviceActivation";

const ACTIVATION_USER_AGENT: &str = "iOS Device Activator (MobileActivation-592.103.2)";
const MAX_DEVICE_FRAME: usize = 16 * 1024 * 1024;
const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_HEADERS: usize = 64 * 1024;
const FORM_VALUE_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'*');

#[derive(Debug)]
pub(crate) struct MobileActivationClient {
    idevice: Idevice,
}

impl IdeviceService for MobileActivationClient {
    fn service_name() -> Cow<'static, str> {
        Cow::Borrowed("com.apple.mobileactivationd")
    }

    async fn from_stream(idevice: Idevice) -> Result<Self, IdeviceError> {
        Ok(Self { idevice })
    }
}

impl MobileActivationClient {
    pub(crate) async fn exchange(
        &mut self,
        request: Dictionary,
    ) -> Result<Dictionary, IdeviceError> {
        let mut bytes = Vec::new();
        Value::Dictionary(request).to_writer_xml(&mut bytes)?;
        if bytes.len() > MAX_DEVICE_FRAME {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "mobileactivationd request exceeds {MAX_DEVICE_FRAME} bytes"
            )));
        }
        self.idevice
            .send_raw(&(bytes.len() as u32).to_be_bytes())
            .await?;
        self.idevice.send_raw(&bytes).await?;

        let length = self.idevice.read_raw(4).await?;
        let length = u32::from_be_bytes(length.try_into().expect("four bytes")) as usize;
        if length == 0 || length > MAX_DEVICE_FRAME {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "invalid mobileactivationd response length {length}"
            )));
        }
        let response = self.idevice.read_raw(length).await?;
        plist::from_bytes(&response).map_err(Into::into)
    }
}

#[derive(Debug)]
pub(crate) struct ActivationServerResponse {
    pub(crate) body: Vec<u8>,
    pub(crate) headers: Dictionary,
}

pub(crate) fn session_info_request() -> Dictionary {
    Dictionary::from_iter([(
        "Command".to_owned(),
        Value::String("CreateTunnel1SessionInfoRequest".into()),
    )])
}

pub(crate) fn activation_info_request(handshake_response: Vec<u8>) -> Dictionary {
    Dictionary::from_iter([
        (
            "Command".to_owned(),
            Value::String("CreateActivationInfoRequest".into()),
        ),
        ("Value".to_owned(), Value::Data(handshake_response)),
        (
            "Options".to_owned(),
            Value::Dictionary(Dictionary::from_iter([(
                "BasebandWaitCount".to_owned(),
                Value::Integer(90_i64.into()),
            )])),
        ),
    ])
}

pub(crate) fn handle_activation_request(response: Vec<u8>, headers: Dictionary) -> Dictionary {
    Dictionary::from_iter([
        (
            "Command".to_owned(),
            Value::String("HandleActivationInfoWithSessionRequest".into()),
        ),
        ("Value".to_owned(), Value::Data(response)),
        (
            "ActivationResponseHeaders".to_owned(),
            Value::Dictionary(headers),
        ),
    ])
}

pub(crate) fn response_value_dictionary(
    response: &Dictionary,
    command: &str,
) -> CoreResult<Dictionary> {
    ensure_response_success(response, command)?;
    response
        .get("Value")
        .and_then(Value::as_dictionary)
        .cloned()
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "mobileactivationd {command} response missing dictionary Value"
            ))
        })
}

pub(crate) fn ensure_response_success(response: &Dictionary, command: &str) -> CoreResult<()> {
    if let Some(error) = response
        .get("ErrorDescription")
        .or_else(|| response.get("Error"))
    {
        return Err(CoreError::DeviceUnavailable(format!(
            "mobileactivationd {command} failed: {error:?}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_session_info(value: &Dictionary) -> CoreResult<()> {
    match value
        .get("HandshakeRequestMessage")
        .and_then(Value::as_data)
    {
        Some(bytes) if !bytes.is_empty() => Ok(()),
        _ => Err(CoreError::Backend(
            "mobileactivationd session info missing HandshakeRequestMessage data".into(),
        )),
    }
}

pub(crate) fn http_client() -> CoreResult<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| CoreError::Backend(format!("build activation HTTP client: {error}")))
}

pub(crate) async fn send_drm_handshake(
    client: &reqwest::Client,
    url: &str,
    session_info: &Dictionary,
) -> CoreResult<Vec<u8>> {
    let body = plist_xml(session_info)?;
    let (_, body) = post_bounded(
        client,
        url,
        "application/x-apple-plist",
        "application/xml",
        body,
        "DRM handshake",
    )
    .await?;
    if body.is_empty() {
        return Err(CoreError::Backend(
            "Apple DRM handshake returned an empty response".into(),
        ));
    }
    Ok(body)
}

pub(crate) async fn send_activation_info(
    client: &reqwest::Client,
    url: &str,
    activation_info: &Dictionary,
) -> CoreResult<ActivationServerResponse> {
    let xml = String::from_utf8(plist_xml(activation_info)?)
        .map_err(|error| CoreError::Backend(format!("activation plist was not UTF-8: {error}")))?;
    let body = format!(
        "activation-info={}",
        utf8_percent_encode(&xml, FORM_VALUE_ENCODE_SET)
    )
    .into_bytes();
    let (headers, body) = post_bounded(
        client,
        url,
        "application/x-www-form-urlencoded",
        "*/*",
        body,
        "device activation",
    )
    .await?;
    let _: Dictionary = plist::from_bytes(&body).map_err(|error| {
        CoreError::Backend(format!(
            "Apple device activation response is not a plist dictionary: {error}"
        ))
    })?;
    Ok(ActivationServerResponse {
        body,
        headers: flatten_headers(&headers)?,
    })
}

async fn post_bounded(
    client: &reqwest::Client,
    url: &str,
    content_type: &'static str,
    accept: &'static str,
    body: Vec<u8>,
    operation: &str,
) -> CoreResult<(HeaderMap, Vec<u8>)> {
    if body.len() > MAX_HTTP_BODY {
        return Err(CoreError::Backend(format!(
            "Apple {operation} request exceeds {MAX_HTTP_BODY} bytes"
        )));
    }
    let response = client
        .post(url)
        .header(CONTENT_TYPE, content_type)
        .header(ACCEPT, accept)
        .header(USER_AGENT, ACTIVATION_USER_AGENT)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!("Apple {operation} request failed: {error}"))
        })?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = read_bounded(response, operation).await?;
    if !status.is_success() {
        return Err(CoreError::DeviceUnavailable(format!(
            "Apple {operation} returned HTTP {status} ({} response bytes)",
            body.len()
        )));
    }
    Ok((headers, body))
}

async fn read_bounded(mut response: reqwest::Response, operation: &str) -> CoreResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HTTP_BODY as u64)
    {
        return Err(CoreError::Backend(format!(
            "Apple {operation} response exceeds {MAX_HTTP_BODY} bytes"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        CoreError::DeviceUnavailable(format!("read Apple {operation} response body: {error}"))
    })? {
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY {
            return Err(CoreError::Backend(format!(
                "Apple {operation} response exceeds {MAX_HTTP_BODY} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn plist_xml(value: &Dictionary) -> CoreResult<Vec<u8>> {
    let mut bytes = Vec::new();
    Value::Dictionary(value.clone())
        .to_writer_xml(&mut bytes)
        .map_err(|error| CoreError::Backend(format!("serialize activation plist: {error}")))?;
    Ok(bytes)
}

fn flatten_headers(headers: &HeaderMap) -> CoreResult<Dictionary> {
    let mut result = Dictionary::new();
    let mut total_bytes = 0_usize;
    for name in headers.keys() {
        let mut last = None;
        for value in headers.get_all(name) {
            let value = value.to_str().map_err(|error| {
                CoreError::Backend(format!(
                    "Apple activation response header {name} is not text: {error}"
                ))
            })?;
            total_bytes = total_bytes.saturating_add(name.as_str().len() + value.len());
            if total_bytes > MAX_RESPONSE_HEADERS {
                return Err(CoreError::Backend(format!(
                    "Apple activation response headers exceed {MAX_RESPONSE_HEADERS} bytes"
                )));
            }
            last = Some(value.to_owned());
        }
        if let Some(value) = last {
            result.insert(canonical_header_name(name.as_str()), Value::String(value));
        }
    }
    Ok(result)
}

fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut bytes = part.bytes();
            match bytes.next() {
                Some(first) => {
                    let mut value = String::with_capacity(part.len());
                    value.push(first.to_ascii_uppercase() as char);
                    value.extend(bytes.map(|byte| byte.to_ascii_lowercase() as char));
                    value
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap as AxumHeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use percent_encoding::percent_decode_str;
    use tokio::sync::Mutex;

    use super::*;

    type CapturedRequest = (String, HeaderMap, Vec<u8>);

    #[derive(Clone, Default)]
    struct TestState {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    async fn handshake(
        State(state): State<TestState>,
        headers: AxumHeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        state
            .requests
            .lock()
            .await
            .push(("handshake".into(), headers, body.to_vec()));
        (StatusCode::OK, b"handshake-response".to_vec())
    }

    async fn activate(
        State(state): State<TestState>,
        headers: AxumHeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        state
            .requests
            .lock()
            .await
            .push(("activate".into(), headers, body.to_vec()));
        let mut response = Vec::new();
        Value::Dictionary(Dictionary::from_iter([(
            "ActivationRecord".to_owned(),
            Value::String("mock-record".into()),
        )]))
        .to_writer_xml(&mut response)
        .unwrap();
        let mut headers = AxumHeaderMap::new();
        headers.insert("x-activation-id", "mock-id".parse().unwrap());
        (StatusCode::OK, headers, response)
    }

    async fn denied() -> impl IntoResponse {
        (StatusCode::BAD_REQUEST, "activation denied")
    }

    async fn invalid_plist() -> impl IntoResponse {
        (StatusCode::OK, "not a plist")
    }

    async fn oversized() -> impl IntoResponse {
        (StatusCode::OK, vec![0_u8; MAX_HTTP_BODY + 1])
    }

    #[tokio::test]
    async fn apple_http_exchange_uses_expected_headers_form_and_plists() {
        let state = TestState::default();
        let app = Router::new()
            .route("/drmHandshake", post(handshake))
            .route("/deviceActivation", post(activate))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = http_client().unwrap();
        let session = Dictionary::from_iter([(
            "HandshakeRequestMessage".to_owned(),
            Value::Data(vec![1, 2, 3]),
        )]);

        let handshake_response =
            send_drm_handshake(&client, &format!("http://{address}/drmHandshake"), &session)
                .await
                .unwrap();
        assert_eq!(handshake_response, b"handshake-response");
        let activation_info = Dictionary::from_iter([(
            "ActivationInfoXML".to_owned(),
            Value::String("mock activation info".into()),
        )]);
        let response = send_activation_info(
            &client,
            &format!("http://{address}/deviceActivation"),
            &activation_info,
        )
        .await
        .unwrap();
        assert_eq!(
            response
                .headers
                .get("X-Activation-Id")
                .and_then(Value::as_string),
            Some("mock-id")
        );
        let _: Dictionary = plist::from_bytes(&response.body).unwrap();

        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "handshake");
        assert_eq!(requests[0].1[CONTENT_TYPE], "application/x-apple-plist");
        let decoded_session: Dictionary = plist::from_bytes(&requests[0].2).unwrap();
        assert_eq!(decoded_session, session);
        assert_eq!(requests[1].0, "activate");
        assert_eq!(
            requests[1].1[CONTENT_TYPE],
            "application/x-www-form-urlencoded"
        );
        let form = std::str::from_utf8(&requests[1].2).unwrap();
        let encoded = form.strip_prefix("activation-info=").unwrap();
        let decoded = percent_decode_str(encoded).decode_utf8().unwrap();
        let decoded_activation: Dictionary = plist::from_bytes(decoded.as_bytes()).unwrap();
        assert_eq!(decoded_activation, activation_info);
        server.abort();
    }

    #[tokio::test]
    async fn apple_http_exchange_rejects_status_errors_and_invalid_plists() {
        let app = Router::new()
            .route("/denied", post(denied))
            .route("/invalid", post(invalid_plist))
            .route("/oversized", post(oversized));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = http_client().unwrap();
        let value =
            Dictionary::from_iter([("HandshakeRequestMessage".to_owned(), Value::Data(vec![1]))]);

        let error = send_drm_handshake(&client, &format!("http://{address}/denied"), &value)
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::DeviceUnavailable(_)));
        assert!(error.to_string().contains("HTTP 400"));
        assert!(!error.to_string().contains("activation denied"));
        let error = send_activation_info(&client, &format!("http://{address}/invalid"), &value)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not a plist dictionary"));
        let error = send_drm_handshake(&client, &format!("http://{address}/oversized"), &value)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 8388608 bytes"));
        server.abort();
    }

    #[test]
    fn device_requests_have_stable_commands_and_validate_responses() {
        let request = session_info_request();
        assert_eq!(
            request.get("Command").and_then(Value::as_string),
            Some("CreateTunnel1SessionInfoRequest")
        );
        let request = activation_info_request(vec![9, 8, 7]);
        assert_eq!(
            request.get("Value").and_then(Value::as_data),
            Some(&[9, 8, 7][..])
        );
        assert_eq!(
            request["Options"].as_dictionary().unwrap()["BasebandWaitCount"].as_signed_integer(),
            Some(90)
        );
        let request = handle_activation_request(
            vec![4, 5, 6],
            Dictionary::from_iter([(
                "Content-Type".to_owned(),
                Value::String("text/xml".to_owned()),
            )]),
        );
        assert_eq!(
            request.get("Command").and_then(Value::as_string),
            Some("HandleActivationInfoWithSessionRequest")
        );

        let session =
            Dictionary::from_iter([("HandshakeRequestMessage".to_owned(), Value::Data(vec![1]))]);
        validate_session_info(&session).unwrap();
        assert!(validate_session_info(&Dictionary::new()).is_err());
        assert!(
            ensure_response_success(
                &Dictionary::from_iter([(
                    "ErrorDescription".to_owned(),
                    Value::String("denied".to_owned()),
                )]),
                "test"
            )
            .is_err()
        );
    }
}
