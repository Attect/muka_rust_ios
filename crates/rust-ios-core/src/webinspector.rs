use std::time::Duration;

use async_trait::async_trait;
use idevice::IdeviceService;
use idevice::provider::IdeviceProvider;
use ios_device_core::webinspector::{
    Application, ApplicationPage, AutomationAvailability, AutomationSession, InspectorSession,
    Page, WebInspectorClient, WebInspectorError, WirType,
};
use rust_ios_protocol::{CommandInvocation, WebInspectorCdpSpec, WebInspectorJsShellSpec};
use serde_json::{Map, Value, json};

use crate::{
    BoxedIo, CoreError, CoreResult, RsdServiceKind, ServiceTransport, UserSpaceTunnel,
    WebInspectorCdpSession, WebInspectorCdpSessionInfo, WebInspectorJsShellSession,
    WebInspectorJsShellSessionInfo, WebInspectorPageInfo,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_IDLE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_TIMEOUT_SECONDS: f64 = 3_600.0;

#[derive(Debug, Clone, PartialEq)]
enum Action {
    List,
    Eval {
        page_key: String,
        expression: String,
        console_enable: bool,
    },
    Launch {
        url: String,
        bundle_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Arguments {
    action: Action,
    timeout: Duration,
}

pub(crate) async fn execute(
    provider: &dyn IdeviceProvider,
    invocation: &CommandInvocation,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Value> {
    let arguments = parse_arguments(&invocation.argv)?;
    let timeout_duration = arguments.timeout;
    tokio::time::timeout(
        timeout_duration,
        execute_with_timeout(provider, tunnel, arguments),
    )
    .await
    .map_err(|_| {
        CoreError::Timeout(format!(
            "webinspector command exceeded {:.3} seconds",
            timeout_duration.as_secs_f64()
        ))
    })?
}

pub(crate) async fn start_cdp_session(
    udid: String,
    provider: &dyn IdeviceProvider,
    spec: &WebInspectorCdpSpec,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Box<dyn WebInspectorCdpSession>> {
    let timeout = Duration::from_millis(spec.startup_timeout_ms.clamp(1_000, 120_000));
    let (stream, transport) = connect_service(provider, tunnel).await?;
    let mut client = WebInspectorClient::new(stream);
    client
        .start(timeout)
        .await
        .map_err(map_webinspector_error)?;
    let pages = client
        .open_application_pages(PAGE_IDLE_TIMEOUT)
        .await
        .map_err(map_webinspector_error)?;
    Ok(Box::new(IdeviceWebInspectorCdpSession {
        info: WebInspectorCdpSessionInfo {
            udid,
            pages: page_infos(&pages),
            service_transport: transport,
        },
        client: Some(client),
        pages,
        inspector: None,
        timeout,
    }))
}

pub(crate) async fn start_js_shell_session(
    udid: String,
    provider: &dyn IdeviceProvider,
    spec: &WebInspectorJsShellSpec,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<Box<dyn WebInspectorJsShellSession>> {
    let timeout = Duration::from_millis(spec.command_timeout_ms.clamp(1_000, 120_000));
    let (stream, transport) = connect_service(provider, tunnel).await?;
    let mut client = WebInspectorClient::new(stream);
    client
        .start(timeout)
        .await
        .map_err(map_webinspector_error)?;

    let bundle_id = if spec.open_safari {
        let safari = ios_device_core::webinspector::SAFARI_BUNDLE_ID;
        client
            .request_application_launch(safari)
            .await
            .map_err(map_webinspector_error)?;
        let _ = wait_for_application(&mut client, safari).await?;
        spec.bundle_id.as_deref().or(Some(safari))
    } else {
        spec.bundle_id.as_deref()
    };
    let pages = client
        .open_application_pages(PAGE_IDLE_TIMEOUT)
        .await
        .map_err(map_webinspector_error)?;
    let selected = select_shell_page(pages, bundle_id)?;
    let page = page_info(&selected);
    let mut inspector = InspectorSession::new(selected.application.id, selected.page.id);
    inspector
        .attach(&mut client, true, timeout)
        .await
        .map_err(map_webinspector_error)?;
    if let Some(url) = spec.url.as_deref() {
        let quoted = serde_json::to_string(url)
            .map_err(|error| CoreError::Backend(format!("serialize WebInspector URL: {error}")))?;
        evaluate_inspector(
            &mut client,
            &mut inspector,
            &format!("window.location = {quoted}"),
            timeout,
        )
        .await?;
    }
    Ok(Box::new(IdeviceWebInspectorJsShellSession {
        info: WebInspectorJsShellSessionInfo {
            udid,
            page,
            service_transport: transport,
        },
        client: Some(client),
        inspector,
        timeout,
    }))
}

#[derive(Debug)]
struct IdeviceWebInspectorCdpSession {
    info: WebInspectorCdpSessionInfo,
    client: Option<WebInspectorClient<BoxedIo>>,
    pages: Vec<ApplicationPage>,
    inspector: Option<InspectorSession>,
    timeout: Duration,
}

#[async_trait]
impl WebInspectorCdpSession for IdeviceWebInspectorCdpSession {
    fn info(&self) -> &WebInspectorCdpSessionInfo {
        &self.info
    }

    async fn refresh_pages(&mut self) -> CoreResult<Vec<WebInspectorPageInfo>> {
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("WebInspector session is closed".into()))?;
        let pages = client
            .open_application_pages(PAGE_IDLE_TIMEOUT)
            .await
            .map_err(map_webinspector_error)?;
        let page_infos = page_infos(&pages);
        self.pages = pages;
        self.info.pages = page_infos.clone();
        Ok(page_infos)
    }

    async fn attach_page(&mut self, page_key: &str) -> CoreResult<()> {
        let selected = self
            .pages
            .iter()
            .find(|candidate| {
                candidate.page.listing_key == page_key
                    && matches!(candidate.page.page_type, WirType::Web | WirType::WebPage)
            })
            .cloned()
            .ok_or_else(|| {
                CoreError::DeviceUnavailable(format!(
                    "no inspectable WebInspector page with key {page_key:?} was found"
                ))
            })?;
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("WebInspector session is closed".into()))?;
        let mut inspector = InspectorSession::new(selected.application.id, selected.page.id);
        inspector
            .attach(client, true, self.timeout)
            .await
            .map_err(map_webinspector_error)?;
        self.inspector = Some(inspector);
        Ok(())
    }

    async fn send_message(&mut self, message: Value) -> CoreResult<()> {
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("WebInspector session is closed".into()))?;
        let inspector = self.inspector.as_mut().ok_or_else(|| {
            CoreError::InvalidArgument("CDP page session has not been attached".into())
        })?;
        inspector
            .send_bridge_message(client, &message)
            .await
            .map_err(map_webinspector_error)
    }

    async fn next_message(&mut self, timeout: Duration) -> CoreResult<Option<Value>> {
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("WebInspector session is closed".into()))?;
        let inspector = self.inspector.as_mut().ok_or_else(|| {
            CoreError::InvalidArgument("CDP page session has not been attached".into())
        })?;
        match inspector.next_raw_message(client, timeout).await {
            Ok(message) => inspector
                .bridge_message(&message)
                .map_err(map_webinspector_error),
            Err(WebInspectorError::Timeout(_)) => Ok(None),
            Err(error) => Err(map_webinspector_error(error)),
        }
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.inspector = None;
        self.client.take();
        Ok(())
    }
}

#[derive(Debug)]
struct IdeviceWebInspectorJsShellSession {
    info: WebInspectorJsShellSessionInfo,
    client: Option<WebInspectorClient<BoxedIo>>,
    inspector: InspectorSession,
    timeout: Duration,
}

#[async_trait]
impl WebInspectorJsShellSession for IdeviceWebInspectorJsShellSession {
    fn info(&self) -> &WebInspectorJsShellSessionInfo {
        &self.info
    }

    async fn evaluate(&mut self, expression: &str) -> CoreResult<Value> {
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| CoreError::DeviceUnavailable("WebInspector shell is closed".into()))?;
        evaluate_inspector(client, &mut self.inspector, expression, self.timeout).await
    }

    async fn close(&mut self) -> CoreResult<()> {
        self.client.take();
        Ok(())
    }
}

fn page_infos(pages: &[ApplicationPage]) -> Vec<WebInspectorPageInfo> {
    let mut result: Vec<_> = pages
        .iter()
        .filter(|candidate| matches!(candidate.page.page_type, WirType::Web | WirType::WebPage))
        .map(page_info)
        .collect();
    result.sort_by(|left, right| {
        left.bundle_id
            .cmp(&right.bundle_id)
            .then_with(|| left.key.cmp(&right.key))
    });
    result
}

fn page_info(candidate: &ApplicationPage) -> WebInspectorPageInfo {
    WebInspectorPageInfo {
        key: candidate.page.listing_key.clone(),
        page_id: candidate.page.id,
        application_id: candidate.application.id.clone(),
        bundle_id: candidate.application.bundle_identifier.clone(),
        title: candidate.page.title.clone().unwrap_or_default(),
        url: candidate.page.url.clone().unwrap_or_default(),
    }
}

fn select_shell_page(
    pages: Vec<ApplicationPage>,
    bundle_id: Option<&str>,
) -> CoreResult<ApplicationPage> {
    let mut candidates: Vec<_> = pages
        .into_iter()
        .filter(|candidate| {
            matches!(
                candidate.page.page_type,
                WirType::Web | WirType::WebPage | WirType::JavaScript
            ) && bundle_id
                .is_none_or(|bundle_id| candidate.application.bundle_identifier == bundle_id)
        })
        .collect();
    sort_pages(&mut candidates);
    candidates.into_iter().next().ok_or_else(|| {
        CoreError::DeviceUnavailable(match bundle_id {
            Some(bundle_id) => {
                format!("no inspectable WebInspector page for bundle {bundle_id:?} was found")
            }
            None => "no inspectable WebInspector page was found".into(),
        })
    })
}

async fn evaluate_inspector(
    client: &mut WebInspectorClient<BoxedIo>,
    inspector: &mut InspectorSession,
    expression: &str,
    timeout: Duration,
) -> CoreResult<Value> {
    let response = inspector
        .send_command_and_wait(
            client,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "objectGroup": "console",
                "includeCommandLineAPI": true,
                "returnByValue": true,
                "generatePreview": true,
                "userGesture": true,
                "awaitPromise": false,
                "replMode": true,
                "allowUnsafeEvalBlockedByCSP": false,
            }),
            timeout,
        )
        .await
        .map_err(map_webinspector_error)?;
    parse_evaluate_result(&response)
}

async fn execute_with_timeout(
    provider: &dyn IdeviceProvider,
    tunnel: Option<&dyn UserSpaceTunnel>,
    arguments: Arguments,
) -> CoreResult<Value> {
    let (stream, _transport) = connect_service(provider, tunnel).await?;
    let mut client = WebInspectorClient::new(stream);
    client
        .start(arguments.timeout)
        .await
        .map_err(map_webinspector_error)?;

    match arguments.action {
        Action::List => {
            let mut pages = client
                .open_application_pages(PAGE_IDLE_TIMEOUT)
                .await
                .map_err(map_webinspector_error)?;
            sort_pages(&mut pages);
            Ok(Value::Array(
                pages.iter().map(application_page_json).collect(),
            ))
        }
        Action::Eval {
            page_key,
            expression,
            console_enable,
        } => {
            let pages = client
                .open_application_pages(PAGE_IDLE_TIMEOUT)
                .await
                .map_err(map_webinspector_error)?;
            let selected = select_page(pages, &page_key)?;
            let mut session = InspectorSession::new(selected.application.id, selected.page.id);
            session
                .attach(&mut client, true, arguments.timeout)
                .await
                .map_err(map_webinspector_error)?;
            if console_enable {
                session
                    .send_command_and_wait(
                        &mut client,
                        "Console.enable",
                        Value::Object(Map::new()),
                        arguments.timeout,
                    )
                    .await
                    .map_err(map_webinspector_error)?;
            }
            let response = session
                .send_command_and_wait(
                    &mut client,
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "objectGroup": "console",
                        "includeCommandLineAPI": true,
                        "returnByValue": true,
                        "generatePreview": true,
                        "userGesture": true,
                        "awaitPromise": false,
                        "replMode": true,
                        "allowUnsafeEvalBlockedByCSP": false,
                    }),
                    arguments.timeout,
                )
                .await
                .map_err(map_webinspector_error)?;
            parse_evaluate_result(&response)
        }
        Action::Launch { url, bundle_id } => {
            if matches!(
                client.automation_availability(),
                Some(AutomationAvailability::NotAvailable)
            ) {
                return Err(remote_automation_disabled());
            }
            client
                .request_application_launch(&bundle_id)
                .await
                .map_err(map_webinspector_error)?;
            let application = wait_for_application(&mut client, &bundle_id).await?;
            let mut session =
                AutomationSession::new(application.id.clone(), application.bundle_identifier);
            session
                .attach(&mut client, arguments.timeout)
                .await
                .map_err(map_webinspector_error)?;
            session
                .start_session(&mut client)
                .await
                .map_err(map_webinspector_error)?;
            session
                .navigate(&mut client, &url)
                .await
                .map_err(map_webinspector_error)?;
            let current_url = session
                .current_url(&mut client)
                .await
                .map_err(map_webinspector_error)?;
            let title = session
                .get_title(&mut client)
                .await
                .map_err(map_webinspector_error)?;
            let _ = session.stop_session(&mut client).await;
            Ok(json!({
                "bundleID": bundle_id,
                "url": current_url,
                "title": title,
            }))
        }
    }
}

async fn connect_service(
    provider: &dyn IdeviceProvider,
    tunnel: Option<&dyn UserSpaceTunnel>,
) -> CoreResult<(BoxedIo, ServiceTransport)> {
    if let Some(tunnel) = tunnel {
        let stream = tunnel
            .connect_rsd_service(RsdServiceKind::WebInspector)
            .await?;
        return Ok((stream, ServiceTransport::SharedRsd));
    }

    let pairing = provider
        .get_pairing_file()
        .await
        .map_err(map_device_error)?;
    let mut lockdown = idevice::lockdown::LockdownClient::connect(provider)
        .await
        .map_err(map_device_error)?;
    let legacy = lockdown
        .start_session(&pairing)
        .await
        .map_err(map_device_error)?;
    let product_version = lockdown
        .get_value(Some("ProductVersion"), None)
        .await
        .map_err(map_device_error)?
        .as_string()
        .map(str::to_owned)
        .ok_or_else(|| {
            CoreError::Backend("Lockdown ProductVersion was not a string".to_string())
        })?;
    if ios_major_version(&product_version)? >= 17 {
        return Err(CoreError::DeviceUnavailable(format!(
            "iOS {product_version} WebInspector requires a running server-owned userspace tunnel; run tunnel start --userspace"
        )));
    }

    let (port, ssl) = lockdown
        .start_service(ios_device_core::webinspector::SERVICE_NAME)
        .await
        .map_err(map_device_error)?;
    let mut stream = provider.connect(port).await.map_err(map_device_error)?;
    if ssl {
        stream
            .start_session(&pairing, legacy)
            .await
            .map_err(map_device_error)?;
    }
    let socket = stream.get_socket().ok_or_else(|| {
        CoreError::DeviceUnavailable("WebInspector service returned no device socket".into())
    })?;
    Ok((Box::new(socket), ServiceTransport::Lockdown))
}

async fn wait_for_application(
    client: &mut WebInspectorClient<BoxedIo>,
    bundle_id: &str,
) -> CoreResult<Application> {
    loop {
        if let Some(application) = client.application_by_bundle(bundle_id) {
            return Ok(application.clone());
        }
        match client
            .next_event_with_timeout(Duration::from_millis(250))
            .await
        {
            Ok(_) | Err(WebInspectorError::Timeout(_)) => {}
            Err(error) => return Err(map_webinspector_error(error)),
        }
    }
}

fn parse_arguments(argv: &[String]) -> CoreResult<Arguments> {
    let action_name = argv.get(1).map(String::as_str).ok_or_else(|| {
        CoreError::InvalidArgument(
            "webinspector requires list, eval, launch, js-shell or cdp".into(),
        )
    })?;
    if matches!(action_name, "js-shell" | "cdp") {
        return Err(CoreError::NotImplemented(format!(
            "webinspector {action_name} requires a server-owned persistent session"
        )));
    }
    if !matches!(action_name, "list" | "eval" | "launch") {
        return Err(CoreError::InvalidArgument(format!(
            "unknown webinspector action {action_name:?}"
        )));
    }

    let mut timeout = DEFAULT_TIMEOUT;
    let mut bundle_id = None;
    let mut console_enable = false;
    let mut positional = Vec::new();
    let mut positional_only = false;
    let mut index = 2;
    while index < argv.len() {
        let value = &argv[index];
        if positional_only {
            positional.push(value.clone());
            index += 1;
            continue;
        }
        if value == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if value == "--timeout" {
            let raw = argv.get(index + 1).ok_or_else(|| {
                CoreError::InvalidArgument("webinspector --timeout requires a value".into())
            })?;
            timeout = parse_timeout(raw)?;
            index += 2;
            continue;
        }
        if let Some(raw) = value.strip_prefix("--timeout=") {
            timeout = parse_timeout(raw)?;
            index += 1;
            continue;
        }
        if value == "--bundle-id" {
            bundle_id = Some(
                argv.get(index + 1)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "webinspector --bundle-id requires a value".into(),
                        )
                    })?
                    .clone(),
            );
            index += 2;
            continue;
        }
        if let Some(raw) = value.strip_prefix("--bundle-id=") {
            if raw.is_empty() {
                return Err(CoreError::InvalidArgument(
                    "webinspector --bundle-id requires a value".into(),
                ));
            }
            bundle_id = Some(raw.to_owned());
            index += 1;
            continue;
        }
        if value == "--console-enable" {
            console_enable = true;
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            return Err(CoreError::InvalidArgument(format!(
                "unknown webinspector option {value:?}"
            )));
        }
        positional.push(value.clone());
        index += 1;
    }

    let action = match action_name {
        "list" => {
            reject_action_option(bundle_id.is_some(), "list", "--bundle-id")?;
            reject_action_option(console_enable, "list", "--console-enable")?;
            require_positional_count(&positional, 0, "webinspector list")?;
            Action::List
        }
        "eval" => {
            reject_action_option(bundle_id.is_some(), "eval", "--bundle-id")?;
            require_positional_count(&positional, 2, "webinspector eval")?;
            Action::Eval {
                page_key: positional[0].clone(),
                expression: positional[1].clone(),
                console_enable,
            }
        }
        "launch" => {
            reject_action_option(console_enable, "launch", "--console-enable")?;
            require_positional_count(&positional, 1, "webinspector launch")?;
            Action::Launch {
                url: positional[0].clone(),
                bundle_id: bundle_id
                    .unwrap_or_else(|| ios_device_core::webinspector::SAFARI_BUNDLE_ID.to_owned()),
            }
        }
        _ => unreachable!("validated webinspector action"),
    };
    Ok(Arguments { action, timeout })
}

fn parse_timeout(raw: &str) -> CoreResult<Duration> {
    let seconds = raw.parse::<f64>().map_err(|_| {
        CoreError::InvalidArgument(format!(
            "webinspector --timeout must be a positive number, got {raw:?}"
        ))
    })?;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > MAX_TIMEOUT_SECONDS {
        return Err(CoreError::InvalidArgument(format!(
            "webinspector --timeout must be greater than 0 and at most {MAX_TIMEOUT_SECONDS} seconds"
        )));
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| {
        CoreError::InvalidArgument(format!("invalid webinspector --timeout value {raw:?}"))
    })
}

fn reject_action_option(present: bool, action: &str, option: &str) -> CoreResult<()> {
    if present {
        Err(CoreError::InvalidArgument(format!(
            "webinspector {action} does not accept {option}"
        )))
    } else {
        Ok(())
    }
}

fn require_positional_count(values: &[String], expected: usize, usage: &str) -> CoreResult<()> {
    if values.len() == expected {
        Ok(())
    } else {
        Err(CoreError::InvalidArgument(format!(
            "{usage} expects {expected} positional value(s), got {}",
            values.len()
        )))
    }
}

fn select_page(mut pages: Vec<ApplicationPage>, page_key: &str) -> CoreResult<ApplicationPage> {
    sort_pages(&mut pages);
    pages
        .into_iter()
        .find(|candidate| {
            candidate.page.listing_key == page_key
                && matches!(
                    candidate.page.page_type,
                    WirType::Web | WirType::WebPage | WirType::JavaScript
                )
        })
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "no inspectable WebInspector page with key {page_key:?} was found"
            ))
        })
}

fn sort_pages(pages: &mut [ApplicationPage]) {
    pages.sort_by(|left, right| {
        left.application
            .bundle_identifier
            .cmp(&right.application.bundle_identifier)
            .then_with(|| left.page.listing_key.cmp(&right.page.listing_key))
            .then_with(|| left.page.id.cmp(&right.page.id))
    });
}

fn application_page_json(value: &ApplicationPage) -> Value {
    json!({
        "application": application_json(&value.application),
        "page": page_json(&value.page),
    })
}

fn application_json(application: &Application) -> Value {
    let mut result = Map::from_iter([
        ("id".into(), Value::String(application.id.clone())),
        (
            "bundleId".into(),
            Value::String(application.bundle_identifier.clone()),
        ),
        ("pid".into(), Value::from(application.pid)),
        ("name".into(), Value::String(application.name.clone())),
        (
            "automationAvailability".into(),
            Value::String(automation_availability_wire(&application.availability)),
        ),
        ("active".into(), Value::Bool(application.is_active)),
        ("proxy".into(), Value::Bool(application.is_proxy)),
        ("ready".into(), Value::Bool(application.is_ready)),
    ]);
    if let Some(host) = application
        .host_application_identifier
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        result.insert("host".into(), Value::String(host.clone()));
    }
    Value::Object(result)
}

fn page_json(page: &Page) -> Value {
    let mut result = Map::from_iter([
        ("id".into(), Value::from(page.id)),
        ("key".into(), Value::String(page.listing_key.clone())),
        ("type".into(), Value::String(wir_type_wire(&page.page_type))),
    ]);
    insert_optional_string(&mut result, "url", page.url.as_ref());
    insert_optional_string(&mut result, "title", page.title.as_ref());
    if page.automation_is_paired == Some(true) {
        result.insert("automationIsPaired".into(), Value::Bool(true));
    }
    insert_optional_string(&mut result, "automationName", page.automation_name.as_ref());
    insert_optional_string(
        &mut result,
        "automationVersion",
        page.automation_version.as_ref(),
    );
    insert_optional_string(
        &mut result,
        "automationSessionId",
        page.automation_session_id.as_ref(),
    );
    insert_optional_string(
        &mut result,
        "automationConnectionId",
        page.automation_connection_id.as_ref(),
    );
    Value::Object(result)
}

fn insert_optional_string(result: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        result.insert(key.into(), Value::String(value.clone()));
    }
}

fn automation_availability_wire(value: &AutomationAvailability) -> String {
    match value {
        AutomationAvailability::Available => "WIRAutomationAvailabilityAvailable".into(),
        AutomationAvailability::NotAvailable => "WIRAutomationAvailabilityNotAvailable".into(),
        AutomationAvailability::Unknown(value) => value.clone(),
    }
}

fn wir_type_wire(value: &WirType) -> String {
    match value {
        WirType::Automation => "WIRTypeAutomation".into(),
        WirType::Itml => "WIRTypeITML".into(),
        WirType::JavaScript => "WIRTypeJavaScript".into(),
        WirType::Page => "WIRTypePage".into(),
        WirType::ServiceWorker => "WIRTypeServiceWorker".into(),
        WirType::Web => "WIRTypeWeb".into(),
        WirType::WebPage => "WIRTypeWebPage".into(),
        WirType::AutomaticallyPause => "WIRAutomaticallyPause".into(),
        WirType::Unknown(value) => value.clone(),
    }
}

fn parse_evaluate_result(message: &Value) -> CoreResult<Value> {
    if let Some(error) = message.get("error") {
        return Err(CoreError::Backend(format!(
            "WebInspector evaluate command failed: {error}"
        )));
    }
    let remote = message
        .get("result")
        .and_then(Value::as_object)
        .and_then(|result| result.get("result"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            CoreError::Backend(
                "WebInspector Runtime.evaluate response is missing result.result".into(),
            )
        })?;
    if remote.get("subtype").and_then(Value::as_str) == Some("error") {
        let description = remote
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("JavaScript evaluation failed");
        return Err(CoreError::Backend(format!(
            "WebInspector evaluate error: {description}"
        )));
    }
    if let Some(value) = remote.get("value") {
        return Ok(value.clone());
    }
    if remote.get("type").and_then(Value::as_str) == Some("object")
        && let Some(preview) = remote.get("preview").and_then(Value::as_object)
    {
        return Ok(Value::String(format_object_preview(
            remote
                .get("className")
                .and_then(Value::as_str)
                .unwrap_or("Object"),
            preview,
        )));
    }
    if let Some(description) = remote.get("description").and_then(Value::as_str) {
        return Ok(Value::String(description.to_owned()));
    }
    Ok(Value::Null)
}

fn format_object_preview(class_name: &str, preview: &Map<String, Value>) -> String {
    let class_name = if class_name.is_empty() {
        "Object"
    } else {
        class_name
    };
    let mut result = format!("[object {class_name}]\n{{");
    if let Some(properties) = preview.get("properties").and_then(Value::as_array) {
        for property in properties.iter().filter_map(Value::as_object) {
            let Some(name) = property
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let value = property
                .get("value")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("NOT_SUPPORTED_FOR_PREVIEW");
            result.push_str(&format!("\n\t{name}: {value}"));
            if let Some(property_type) = property
                .get("type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                result.push_str(&format!(", // {property_type}"));
            }
        }
    }
    if preview
        .get("overflow")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        result.push_str("\n\t// ...");
    }
    result.push_str("\n}");
    result
}

fn ios_major_version(version: &str) -> CoreResult<u32> {
    version
        .split('.')
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "Lockdown ProductVersion {version:?} has an invalid format"
            ))
        })
}

fn remote_automation_disabled() -> CoreError {
    CoreError::DeviceUnavailable(
        "Remote Automation is disabled; enable Settings > Safari > Advanced > Remote Automation, then retry"
            .into(),
    )
}

fn map_webinspector_error(error: WebInspectorError) -> CoreError {
    match error {
        WebInspectorError::Timeout(duration) => CoreError::Timeout(format!(
            "waiting for WebInspector response after {:.3} seconds",
            duration.as_secs_f64()
        )),
        WebInspectorError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            CoreError::DeviceUnavailable(
                "Web Inspector is unavailable; enable Settings > Safari > Advanced > Web Inspector, reconnect the device, then retry"
                    .into(),
            )
        }
        WebInspectorError::Io(error) => CoreError::Io(error),
        WebInspectorError::Protocol(message)
            if message == "remote automation is not available" =>
        {
            remote_automation_disabled()
        }
        other => CoreError::Backend(format!("WebInspector protocol failed: {other}")),
    }
}

fn map_device_error(error: idevice::IdeviceError) -> CoreError {
    CoreError::DeviceUnavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn application_page(key: &str, page_type: WirType) -> ApplicationPage {
        ApplicationPage {
            application: Application {
                id: "PID:42".into(),
                bundle_identifier: "com.apple.mobilesafari".into(),
                pid: 42,
                name: "Safari".into(),
                availability: AutomationAvailability::Available,
                is_active: true,
                is_proxy: false,
                is_ready: true,
                host_application_identifier: None,
            },
            page: Page {
                id: 7,
                listing_key: key.into(),
                page_type,
                title: Some("Example".into()),
                url: Some("https://example.com/".into()),
                automation_is_paired: None,
                automation_name: None,
                automation_version: None,
                automation_session_id: None,
                automation_connection_id: None,
            },
        }
    }

    #[test]
    fn parses_compatible_list_eval_and_launch_arguments() {
        assert_eq!(
            parse_arguments(&strings(&["webinspector", "list", "--timeout=0.75"])).unwrap(),
            Arguments {
                action: Action::List,
                timeout: Duration::from_millis(750),
            }
        );
        assert_eq!(
            parse_arguments(&strings(&[
                "webinspector",
                "eval",
                "page-1",
                "document.title",
                "--console-enable",
            ]))
            .unwrap()
            .action,
            Action::Eval {
                page_key: "page-1".into(),
                expression: "document.title".into(),
                console_enable: true,
            }
        );
        assert_eq!(
            parse_arguments(&strings(&[
                "webinspector",
                "launch",
                "https://example.com/",
                "--bundle-id=com.example.browser",
            ]))
            .unwrap()
            .action,
            Action::Launch {
                url: "https://example.com/".into(),
                bundle_id: "com.example.browser".into(),
            }
        );
    }

    #[test]
    fn parser_rejects_ambiguous_or_persistent_actions() {
        assert!(matches!(
            parse_arguments(&strings(&["webinspector", "eval", "page-1"])),
            Err(CoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            parse_arguments(&strings(&["webinspector", "list", "--timeout=0"])),
            Err(CoreError::InvalidArgument(_))
        ));
        assert!(matches!(
            parse_arguments(&strings(&["webinspector", "cdp"])),
            Err(CoreError::NotImplemented(_))
        ));
    }

    #[test]
    fn page_output_matches_go_ios_field_names() {
        let value = application_page_json(&application_page("page-1", WirType::WebPage));
        assert_eq!(value["application"]["bundleId"], "com.apple.mobilesafari");
        assert_eq!(
            value["application"]["automationAvailability"],
            "WIRAutomationAvailabilityAvailable"
        );
        assert_eq!(value["page"]["key"], "page-1");
        assert_eq!(value["page"]["type"], "WIRTypeWebPage");
        assert!(value["application"].get("bundle_identifier").is_none());
    }

    #[test]
    fn page_selection_uses_listing_key_and_supported_types() {
        let selected = select_page(
            vec![
                application_page("automation", WirType::Automation),
                application_page("page-1", WirType::Web),
            ],
            "page-1",
        )
        .unwrap();
        assert_eq!(selected.page.listing_key, "page-1");
        assert!(
            select_page(
                vec![application_page("automation", WirType::Automation)],
                "automation"
            )
            .is_err()
        );
    }

    #[test]
    fn shell_page_selection_supports_javascript_and_bundle_filtering() {
        let mut javascript = application_page("javascript", WirType::JavaScript);
        javascript.application.bundle_identifier = "com.example.javascript".into();
        let selected = select_shell_page(
            vec![application_page("safari", WirType::WebPage), javascript],
            Some("com.example.javascript"),
        )
        .unwrap();
        assert_eq!(selected.page.listing_key, "javascript");
        assert!(
            select_shell_page(
                vec![application_page("automation", WirType::Automation)],
                None
            )
            .is_err()
        );
    }

    #[test]
    fn evaluate_result_returns_values_and_surfaces_remote_errors() {
        assert_eq!(
            parse_evaluate_result(&json!({
                "id": 1,
                "result": { "result": { "type": "number", "value": 4 } }
            }))
            .unwrap(),
            json!(4)
        );
        assert!(
            parse_evaluate_result(&json!({
                "id": 1,
                "result": {
                    "result": { "type": "object", "subtype": "error", "description": "boom" }
                }
            }))
            .unwrap_err()
            .to_string()
            .contains("boom")
        );
        assert_eq!(
            parse_evaluate_result(&json!({
                "id": 1,
                "result": {
                    "result": {
                        "type": "object",
                        "className": "Object",
                        "description": "Object",
                        "preview": {
                            "properties": [
                                { "name": "answer", "type": "number", "value": "42" }
                            ],
                            "overflow": true
                        }
                    }
                }
            }))
            .unwrap(),
            Value::String("[object Object]\n{\n\tanswer: 42, // number\n\t// ...\n}".into())
        );
    }
}
