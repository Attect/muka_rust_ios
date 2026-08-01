use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::{Client, Method};
use rust_ios_protocol::{CommandInvocation, CommandResult};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::signing::{SignedApp, extract_zip, find_app_bundle, sign_with_files};
use crate::{CoreError, CoreResult};

const DEFAULT_DRIVER: &str = "devicekit";
const DEFAULT_WDA_URL: &str = "http://127.0.0.1:8100";
const DEFAULT_DEVICEKIT_URL: &str = "http://127.0.0.1:12004";
const DEFAULT_WDA_ARTIFACT_URL: &str = "https://deviceboxhq.com/WebDriverAgentRunner-13.2.0.zip";
const DEFAULT_DEVICEKIT_ARTIFACT_URL: &str =
    "https://deviceboxhq.com/devicekit-ios-runner-0.0.18.ipa";
const DEFAULT_WDA_BUNDLE_ID: &str = "com.deviceboxhq.goios.WebDriverAgentRunner.xctrunner";
const DEFAULT_DEVICEKIT_BUNDLE_ID: &str = "com.deviceboxhq.goios.devicekit.runner";
const REQUEST_TIMEOUT_SECONDS: u64 = 60;
const DOWNLOAD_TIMEOUT_SECONDS: u64 = 600;
const COMMON_OPTIONS: &[&str] = &["--driver", "--wda-url", "--devicekit-url", "--session-id"];
const VALUE_OPTIONS: &[&str] = &[
    "--driver",
    "--wda-url",
    "--devicekit-url",
    "--session-id",
    "--method",
    "--http-path",
    "--body",
    "--body-file",
    "--rpc-method",
    "--params",
    "--params-file",
    "--x",
    "--y",
    "--from-x",
    "--from-y",
    "--to-x",
    "--to-y",
    "--duration",
    "--text",
    "--output",
    "--orientation",
    "--bundle-id",
    "--bundleid",
    "--path",
    "--p12file",
    "--profile",
    "--p12password",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiArtifactTarget {
    Wda,
    DeviceKit,
}

impl UiArtifactTarget {
    fn from_name(value: &str) -> CoreResult<Self> {
        match value.to_ascii_lowercase().as_str() {
            "wda" => Ok(Self::Wda),
            "devicekit" => Ok(Self::DeviceKit),
            _ => Err(CoreError::InvalidArgument(format!(
                "unknown UI artifact target {value:?}; use wda, devicekit or all"
            ))),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Wda => "wda",
            Self::DeviceKit => "devicekit",
        }
    }

    fn url(self) -> String {
        match self {
            Self::Wda => std::env::var("RUST_IOS_WDA_ARTIFACT_URL")
                .unwrap_or_else(|_| DEFAULT_WDA_ARTIFACT_URL.into()),
            Self::DeviceKit => std::env::var("RUST_IOS_DEVICEKIT_ARTIFACT_URL")
                .unwrap_or_else(|_| DEFAULT_DEVICEKIT_ARTIFACT_URL.into()),
        }
    }

    fn bundle_id(self) -> &'static str {
        match self {
            Self::Wda => DEFAULT_WDA_BUNDLE_ID,
            Self::DeviceKit => DEFAULT_DEVICEKIT_BUNDLE_ID,
        }
    }

    fn output_base_name(self) -> &'static str {
        match self {
            Self::Wda => "WebDriverAgentRunner",
            Self::DeviceKit => "devicekit-ios-runner",
        }
    }
}

#[derive(Debug)]
pub(crate) struct UiInstallResult {
    pub target: &'static str,
    pub artifact_path: PathBuf,
    pub signed: SignedApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiDriver {
    Wda,
    DeviceKit,
}

impl UiDriver {
    fn as_str(self) -> &'static str {
        match self {
            Self::Wda => "wda",
            Self::DeviceKit => "devicekit",
        }
    }
}

#[derive(Debug)]
struct ParsedUiArgs {
    action: String,
    positionals: Vec<String>,
    options: BTreeMap<String, String>,
}

#[derive(Debug)]
struct UiHttpResponse {
    status: reqwest::StatusCode,
    content_type: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct UiClient {
    driver: UiDriver,
    wda_url: String,
    devicekit_url: String,
    http: Client,
    session_id: Option<String>,
}

pub(crate) async fn execute(invocation: &CommandInvocation) -> CoreResult<Value> {
    let action = invocation
        .argv
        .get(1)
        .filter(|value| !value.starts_with('-'))
        .map(String::as_str);
    match action {
        Some("download") => {
            return Ok(download(invocation)
                .await?
                .value
                .expect("UI download returns a value"));
        }
        Some(action @ ("install" | "run")) => {
            return Err(CoreError::NotImplemented(format!(
                "ui {} requires the artifact signing/install runner workflow and is not implemented",
                action
            )));
        }
        Some("stream") => {
            return Err(CoreError::NotImplemented(
                "ui stream must be owned by the server task manager and is not implemented".into(),
            ));
        }
        _ => {}
    }

    let arguments = parse_arguments(&invocation.argv)?;
    let mut client = UiClient::new(&arguments).await?;
    match arguments.action.as_str() {
        "status" => {
            require_positionals(&arguments, 0, "ui status")?;
            validate_options(&arguments, &[])?;
            client.status().await
        }
        "api" | "raw" => {
            require_positionals(&arguments, 0, "ui api/raw")?;
            validate_options(
                &arguments,
                &[
                    "--method",
                    "--http-path",
                    "--body",
                    "--body-file",
                    "--rpc-method",
                    "--params",
                    "--params-file",
                ],
            )?;
            client.raw_api(&arguments, invocation).await
        }
        "tap" => {
            require_positionals(&arguments, 0, "ui tap")?;
            validate_options(&arguments, &["--x", "--y"])?;
            let x = integer_option(&arguments, "--x")?;
            let y = integer_option(&arguments, "--y")?;
            client.tap(x, y).await
        }
        "swipe" => {
            require_positionals(&arguments, 0, "ui swipe")?;
            validate_options(
                &arguments,
                &["--from-x", "--from-y", "--to-x", "--to-y", "--duration"],
            )?;
            client
                .swipe(
                    integer_option(&arguments, "--from-x")?,
                    integer_option(&arguments, "--from-y")?,
                    integer_option(&arguments, "--to-x")?,
                    integer_option(&arguments, "--to-y")?,
                    optional_nonnegative_float(&arguments, "--duration", 0.0)?,
                )
                .await
        }
        "longpress" => {
            require_positionals(&arguments, 0, "ui longpress")?;
            validate_options(&arguments, &["--x", "--y", "--duration"])?;
            client
                .long_press(
                    integer_option(&arguments, "--x")?,
                    integer_option(&arguments, "--y")?,
                    optional_nonnegative_float(&arguments, "--duration", 1.0)?,
                )
                .await
        }
        "type" => {
            require_positionals(&arguments, 0, "ui type")?;
            validate_options(&arguments, &["--text"])?;
            client
                .type_text(required_option(&arguments, "--text")?)
                .await
        }
        "button" => {
            require_positionals(&arguments, 1, "ui button <button>")?;
            validate_options(&arguments, &[])?;
            client.button(&arguments.positionals[0]).await
        }
        "screenshot" => {
            require_positionals(&arguments, 0, "ui screenshot")?;
            validate_options(&arguments, &["--output"])?;
            let output = required_option(&arguments, "--output").map_err(|_| {
                CoreError::NotImplemented(
                    "ui screenshot without --output requires binary stdout transport".into(),
                )
            })?;
            if output == "-" {
                return Err(CoreError::NotImplemented(
                    "ui screenshot --output=- requires binary stdout transport".into(),
                ));
            }
            client.screenshot(invocation, output).await
        }
        "source" => {
            require_positionals(&arguments, 0, "ui source")?;
            validate_options(&arguments, &["--output"])?;
            client
                .source(invocation, arguments.options.get("--output"))
                .await
        }
        "size" => {
            require_positionals(&arguments, 0, "ui size")?;
            validate_options(&arguments, &[])?;
            client.size().await
        }
        "orientation" => {
            validate_options(&arguments, &["--orientation"])?;
            client.orientation(&arguments).await
        }
        "app" => {
            validate_options(&arguments, &["--bundle-id"])?;
            client.app(&arguments).await
        }
        action => Err(CoreError::InvalidArgument(format!(
            "unknown ui action {action:?}"
        ))),
    }
}

pub(crate) async fn download(invocation: &CommandInvocation) -> CoreResult<CommandResult> {
    let arguments = parse_arguments(&invocation.argv)?;
    validate_options(&arguments, &["--output"])?;
    if arguments.positionals.len() > 1 {
        return Err(CoreError::InvalidArgument(
            "ui download accepts at most one of wda, devicekit or all".into(),
        ));
    }
    let targets = match arguments.positionals.first().map(String::as_str) {
        None | Some("all") => vec![UiArtifactTarget::Wda, UiArtifactTarget::DeviceKit],
        Some(value) => vec![UiArtifactTarget::from_name(value)?],
    };
    let output_dir = arguments
        .options
        .get("--output")
        .map(|value| resolve_client_path(invocation, value))
        .unwrap_or_else(|| {
            invocation
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        });
    tokio::fs::create_dir_all(&output_dir).await?;
    let output_dir = tokio::fs::canonicalize(&output_dir).await?;
    let client = Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| CoreError::Backend(format!("build artifact HTTP client: {error}")))?;
    let mut downloaded = Vec::with_capacity(targets.len());
    for target in targets {
        downloaded.push(download_target(&client, target, &output_dir).await?);
    }
    Ok(CommandResult {
        exit_code: 0,
        value: Some(json!({
            "outputDir": output_dir,
            "downloaded": downloaded,
        })),
        text: None,
        warnings: Vec::new(),
    })
}

pub(crate) async fn prepare_install(
    invocation: &CommandInvocation,
    device_udid: Option<&str>,
) -> CoreResult<UiInstallResult> {
    let arguments = parse_arguments(&invocation.argv)?;
    require_positionals(&arguments, 1, "ui install <wda|devicekit>")?;
    validate_options(
        &arguments,
        &[
            "--path",
            "--output",
            "--bundleid",
            "--p12file",
            "--profile",
            "--p12password",
        ],
    )?;
    reject_present(
        &arguments,
        &["--driver", "--wda-url", "--devicekit-url", "--session-id"],
        "ui install",
    )?;
    let target = UiArtifactTarget::from_name(&arguments.positionals[0])?;
    let temporary = tempfile::tempdir()?;
    let artifact_path = if let Some(path) = arguments.options.get("--path") {
        resolve_client_path(invocation, path)
    } else {
        let client = Client::builder()
            .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECONDS))
            .build()
            .map_err(|error| CoreError::Backend(format!("build artifact HTTP client: {error}")))?;
        let url = target.url();
        let file_name = artifact_file_name(&url)?;
        let path = temporary.path().join(file_name);
        download_file(&client, &url, &path).await?;
        path
    };
    if !artifact_path.exists() {
        return Err(CoreError::InvalidArgument(format!(
            "UI artifact does not exist: {}",
            artifact_path.display()
        )));
    }
    let signing_input = if artifact_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        let extracted = temporary.path().join("extracted");
        tokio::fs::create_dir_all(&extracted).await?;
        let archive = artifact_path.clone();
        let extracted_for_worker = extracted.clone();
        tokio::task::spawn_blocking(move || extract_zip(&archive, &extracted_for_worker))
            .await
            .map_err(|error| CoreError::Backend(format!("unzip worker failed: {error}")))??;
        find_app_bundle(&extracted)?
    } else {
        artifact_path.clone()
    };
    let p12_path = resolve_client_path(invocation, required_option(&arguments, "--p12file")?);
    let profile_path = resolve_client_path(invocation, required_option(&arguments, "--profile")?);
    let extension = signing_input
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("app");
    let output_path = arguments
        .options
        .get("--output")
        .map(|value| resolve_client_path(invocation, value))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "{}-signed-{}.{}",
                target.output_base_name(),
                unix_time_ms(),
                extension
            ))
        });
    let bundle_id = arguments
        .options
        .get("--bundleid")
        .cloned()
        .unwrap_or_else(|| target.bundle_id().into());
    let password = arguments
        .options
        .get("--p12password")
        .cloned()
        .or_else(|| std::env::var("P12_PASSWORD").ok())
        .unwrap_or_default();
    let signed = sign_with_files(
        signing_input,
        output_path,
        p12_path,
        password,
        profile_path,
        Some(bundle_id),
        device_udid.map(str::to_owned),
    )
    .await?;
    Ok(UiInstallResult {
        target: target.name(),
        artifact_path,
        signed,
    })
}

async fn download_target(
    client: &Client,
    target: UiArtifactTarget,
    output_dir: &Path,
) -> CoreResult<Value> {
    let url = target.url();
    download_target_from_url(client, target, output_dir, &url).await
}

async fn download_target_from_url(
    client: &Client,
    target: UiArtifactTarget,
    output_dir: &Path,
    url: &str,
) -> CoreResult<Value> {
    let artifact_path = output_dir.join(artifact_file_name(url)?);
    let size_bytes = download_file(client, url, &artifact_path).await?;
    let artifact_type = artifact_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let app_path = if artifact_type == "zip" {
        let extract_dir =
            output_dir.join(format!("{}-{}", target.output_base_name(), unix_time_ms()));
        tokio::fs::create_dir_all(&extract_dir).await?;
        let archive = artifact_path.clone();
        let worker_dir = extract_dir.clone();
        tokio::task::spawn_blocking(move || extract_zip(&archive, &worker_dir))
            .await
            .map_err(|error| CoreError::Backend(format!("unzip worker failed: {error}")))??;
        Some(find_app_bundle(&extract_dir)?)
    } else {
        None
    };
    Ok(json!({
        "target": target.name(),
        "url": url,
        "artifactPath": artifact_path,
        "artifactType": artifact_type,
        "sizeBytes": size_bytes,
        "appPath": app_path,
    }))
}

async fn download_file(client: &Client, url: &str, output_path: &Path) -> CoreResult<u64> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| CoreError::DeviceUnavailable(format!("download {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(CoreError::DeviceUnavailable(format!(
            "download {url} returned HTTP {}",
            response.status()
        )));
    }
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary_path = output_path.with_extension(format!(
        "{}.part-{}",
        output_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("download"),
        uuid::Uuid::new_v4()
    ));
    let result = async {
        let mut output = tokio::fs::File::create(&temporary_path).await?;
        let mut written = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("read download {url}: {error}"))
        })? {
            output.write_all(&chunk).await?;
            written = written.saturating_add(chunk.len() as u64);
        }
        output.flush().await?;
        drop(output);
        if output_path.exists() {
            tokio::fs::remove_file(output_path).await?;
        }
        tokio::fs::rename(&temporary_path, output_path).await?;
        Ok::<u64, CoreError>(written)
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary_path).await;
    }
    result
}

fn artifact_file_name(url: &str) -> CoreResult<String> {
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        CoreError::InvalidArgument(format!("UI artifact URL is invalid: {error}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CoreError::InvalidArgument(
            "UI artifact URL must use http or https".into(),
        ));
    }
    parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|value| !value.is_empty()))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CoreError::InvalidArgument("UI artifact URL has no file name".into()))
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

impl UiClient {
    async fn new(arguments: &ParsedUiArgs) -> CoreResult<Self> {
        let requested_driver = arguments
            .options
            .get("--driver")
            .cloned()
            .or_else(|| std::env::var("GO_IOS_UI_DRIVER").ok())
            .unwrap_or_else(|| DEFAULT_DRIVER.into())
            .to_ascii_lowercase();
        let wda_url = normalized_base_url(
            arguments
                .options
                .get("--wda-url")
                .cloned()
                .or_else(|| std::env::var("GO_IOS_WDA_URL").ok())
                .unwrap_or_else(|| DEFAULT_WDA_URL.into()),
            "--wda-url",
        )?;
        let devicekit_url = normalized_base_url(
            arguments
                .options
                .get("--devicekit-url")
                .cloned()
                .or_else(|| std::env::var("GO_IOS_DEVICEKIT_URL").ok())
                .unwrap_or_else(|| DEFAULT_DEVICEKIT_URL.into()),
            "--devicekit-url",
        )?;
        let http = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECONDS))
            .build()
            .map_err(|error| CoreError::Backend(format!("build UI HTTP client: {error}")))?;
        let driver = match requested_driver.as_str() {
            "wda" => UiDriver::Wda,
            "devicekit" => UiDriver::DeviceKit,
            "auto" => {
                if healthy(&http, &devicekit_url, "/health").await {
                    UiDriver::DeviceKit
                } else if healthy(&http, &wda_url, "/status").await {
                    UiDriver::Wda
                } else {
                    return Err(CoreError::DeviceUnavailable(
                        "no UI backend is reachable; start DeviceKit or WDA, or pass --driver and --*-url"
                            .into(),
                    ));
                }
            }
            value => {
                return Err(CoreError::InvalidArgument(format!(
                    "--driver must be wda, devicekit or auto, got {value:?}"
                )));
            }
        };
        Ok(Self {
            driver,
            wda_url,
            devicekit_url,
            http,
            session_id: arguments.options.get("--session-id").cloned(),
        })
    }

    async fn status(&self) -> CoreResult<Value> {
        let response = match self.driver {
            UiDriver::DeviceKit => self.devicekit_http(Method::GET, "/health", None).await?,
            UiDriver::Wda => self.wda_http(Method::GET, "/status", None).await?,
        };
        response_value(response)
    }

    async fn raw_api(
        &self,
        arguments: &ParsedUiArgs,
        invocation: &CommandInvocation,
    ) -> CoreResult<Value> {
        let response = match self.driver {
            UiDriver::DeviceKit => {
                let method = required_option(arguments, "--rpc-method")?;
                reject_present(
                    arguments,
                    &["--method", "--http-path", "--body", "--body-file"],
                    "DeviceKit ui api/raw",
                )?;
                let params =
                    json_option_or_file(arguments, invocation, "--params", "--params-file")?
                        .unwrap_or_else(|| json!({}));
                self.devicekit_rpc(method, params).await?
            }
            UiDriver::Wda => {
                let path = required_option(arguments, "--http-path")?;
                reject_present(
                    arguments,
                    &["--rpc-method", "--params", "--params-file"],
                    "WDA ui api/raw",
                )?;
                let method = arguments
                    .options
                    .get("--method")
                    .map(String::as_str)
                    .unwrap_or("GET")
                    .parse::<Method>()
                    .map_err(|_| {
                        CoreError::InvalidArgument("--method is not a valid HTTP method".into())
                    })?;
                let body = raw_option_or_file(arguments, invocation, "--body", "--body-file")?;
                self.wda_http(method, path, body).await?
            }
        };
        response_value(response)
    }

    async fn tap(&mut self, x: i64, y: i64) -> CoreResult<Value> {
        match self.driver {
            UiDriver::DeviceKit => response_value(
                self.devicekit_rpc("device.io.tap", json!({ "x": x, "y": y }))
                    .await?,
            ),
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_http(
                        Method::POST,
                        &format!("/session/{session}/wda/tap/{x}/{y}"),
                        None,
                    )
                    .await?,
                )
            }
        }
    }

    async fn swipe(
        &mut self,
        from_x: i64,
        from_y: i64,
        to_x: i64,
        to_y: i64,
        duration: f64,
    ) -> CoreResult<Value> {
        let params = json!({
            "fromX": from_x,
            "fromY": from_y,
            "toX": to_x,
            "toY": to_y,
            "duration": duration,
        });
        match self.driver {
            UiDriver::DeviceKit => {
                response_value(self.devicekit_rpc("device.io.swipe", params).await?)
            }
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_json(
                        Method::POST,
                        &format!("/session/{session}/wda/dragfromtoforduration"),
                        params,
                    )
                    .await?,
                )
            }
        }
    }

    async fn long_press(&mut self, x: i64, y: i64, duration: f64) -> CoreResult<Value> {
        let params = json!({ "x": x, "y": y, "duration": duration });
        match self.driver {
            UiDriver::DeviceKit => {
                response_value(self.devicekit_rpc("device.io.longpress", params).await?)
            }
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_json(
                        Method::POST,
                        &format!("/session/{session}/wda/touchAndHold"),
                        params,
                    )
                    .await?,
                )
            }
        }
    }

    async fn type_text(&mut self, text: &str) -> CoreResult<Value> {
        match self.driver {
            UiDriver::DeviceKit => response_value(
                self.devicekit_rpc("device.io.text", json!({ "text": text }))
                    .await?,
            ),
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_json(
                        Method::POST,
                        &format!("/session/{session}/keys"),
                        json!({ "text": text, "value": [text] }),
                    )
                    .await?,
                )
            }
        }
    }

    async fn button(&mut self, button: &str) -> CoreResult<Value> {
        match self.driver {
            UiDriver::DeviceKit => response_value(
                self.devicekit_rpc("device.io.button", json!({ "button": button }))
                    .await?,
            ),
            UiDriver::Wda if button.eq_ignore_ascii_case("home") => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_http(
                        Method::POST,
                        &format!("/session/{session}/wda/homescreen"),
                        None,
                    )
                    .await?,
                )
            }
            UiDriver::Wda => Err(CoreError::InvalidArgument(
                "WDA only supports the home button; use --driver=devicekit for other buttons"
                    .into(),
            )),
        }
    }

    async fn screenshot(
        &mut self,
        invocation: &CommandInvocation,
        output: &str,
    ) -> CoreResult<Value> {
        let response = match self.driver {
            UiDriver::DeviceKit => self.devicekit_rpc("device.screenshot", json!({})).await?,
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                self.wda_http(Method::GET, &format!("/session/{session}/screenshot"), None)
                    .await?
            }
        };
        let decoded: Value = serde_json::from_slice(&response.body).map_err(|error| {
            CoreError::DeviceUnavailable(format!("UI screenshot response is not JSON: {error}"))
        })?;
        let encoded = find_nested_string(
            &decoded,
            &["value", "result", "image", "screenshot", "data"],
        )
        .ok_or_else(|| {
            CoreError::DeviceUnavailable("UI screenshot response contains no base64 image".into())
        })?;
        let image = BASE64.decode(encoded).map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "UI screenshot response contains invalid base64: {error}"
            ))
        })?;
        let path = resolve_client_path(invocation, output);
        write_output(&path, &image).await?;
        Ok(json!({
            "driver": self.driver.as_str(),
            "output": path,
            "bytes": image.len(),
        }))
    }

    async fn source(
        &mut self,
        invocation: &CommandInvocation,
        output: Option<&String>,
    ) -> CoreResult<Value> {
        let response = match self.driver {
            UiDriver::DeviceKit => self.devicekit_rpc("device.dump.ui", json!({})).await?,
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                self.wda_http(Method::GET, &format!("/session/{session}/source"), None)
                    .await?
            }
        };
        if let Some(output) = output {
            if output == "-" {
                return response_value(response);
            }
            let path = resolve_client_path(invocation, output);
            write_output(&path, &response.body).await?;
            return Ok(json!({
                "driver": self.driver.as_str(),
                "output": path,
                "bytes": response.body.len(),
            }));
        }
        response_value(response)
    }

    async fn size(&mut self) -> CoreResult<Value> {
        match self.driver {
            UiDriver::DeviceKit => {
                response_value(self.devicekit_rpc("device.info", json!({})).await?)
            }
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_http(
                        Method::GET,
                        &format!("/session/{session}/window/size"),
                        None,
                    )
                    .await?,
                )
            }
        }
    }

    async fn orientation(&mut self, arguments: &ParsedUiArgs) -> CoreResult<Value> {
        let operation = arguments
            .positionals
            .first()
            .map(String::as_str)
            .unwrap_or("get");
        match operation {
            "get" if arguments.positionals.len() == 1 || arguments.positionals.is_empty() => {
                match self.driver {
                    UiDriver::DeviceKit => response_value(
                        self.devicekit_rpc("device.io.orientation.get", json!({}))
                            .await?,
                    ),
                    UiDriver::Wda => {
                        let session = self.wda_session().await?;
                        response_value(
                            self.wda_http(
                                Method::GET,
                                &format!("/session/{session}/orientation"),
                                None,
                            )
                            .await?,
                        )
                    }
                }
            }
            "set" if arguments.positionals.len() <= 2 => {
                let orientation = arguments
                    .positionals
                    .get(1)
                    .map(String::as_str)
                    .or_else(|| arguments.options.get("--orientation").map(String::as_str))
                    .ok_or_else(|| {
                        CoreError::InvalidArgument(
                            "ui orientation set requires an orientation".into(),
                        )
                    })?;
                match self.driver {
                    UiDriver::DeviceKit => response_value(
                        self.devicekit_rpc(
                            "device.io.orientation.set",
                            json!({ "orientation": orientation }),
                        )
                        .await?,
                    ),
                    UiDriver::Wda => {
                        let session = self.wda_session().await?;
                        response_value(
                            self.wda_json(
                                Method::POST,
                                &format!("/session/{session}/orientation"),
                                json!({ "orientation": orientation }),
                            )
                            .await?,
                        )
                    }
                }
            }
            _ => Err(CoreError::InvalidArgument(
                "ui orientation requires get or set <orientation>".into(),
            )),
        }
    }

    async fn app(&mut self, arguments: &ParsedUiArgs) -> CoreResult<Value> {
        let action = arguments
            .positionals
            .first()
            .map(String::as_str)
            .ok_or_else(|| {
                CoreError::InvalidArgument("ui app requires launch, terminate or foreground".into())
            })?;
        if action == "foreground" {
            require_positionals(arguments, 1, "ui app foreground")?;
            if self.driver != UiDriver::DeviceKit {
                return Err(CoreError::InvalidArgument(
                    "ui app foreground is only available with --driver=devicekit".into(),
                ));
            }
            return response_value(
                self.devicekit_rpc("device.apps.foreground", json!({}))
                    .await?,
            );
        }
        if !matches!(action, "launch" | "terminate") || arguments.positionals.len() > 2 {
            return Err(CoreError::InvalidArgument(
                "ui app requires launch|terminate <bundleID> or foreground".into(),
            ));
        }
        let bundle_id = arguments
            .positionals
            .get(1)
            .map(String::as_str)
            .or_else(|| arguments.options.get("--bundle-id").map(String::as_str))
            .ok_or_else(|| CoreError::InvalidArgument("ui app requires a bundle ID".into()))?;
        match self.driver {
            UiDriver::DeviceKit => response_value(
                self.devicekit_rpc(
                    &format!("device.apps.{action}"),
                    json!({ "bundleId": bundle_id }),
                )
                .await?,
            ),
            UiDriver::Wda => {
                let session = self.wda_session().await?;
                response_value(
                    self.wda_json(
                        Method::POST,
                        &format!("/session/{session}/wda/apps/{action}"),
                        json!({ "bundleId": bundle_id }),
                    )
                    .await?,
                )
            }
        }
    }

    async fn wda_session(&mut self) -> CoreResult<String> {
        if let Some(session_id) = &self.session_id {
            return Ok(session_id.clone());
        }
        let response = self
            .wda_json(
                Method::POST,
                "/session",
                json!({ "capabilities": {}, "desiredCapabilities": {} }),
            )
            .await?;
        let value: Value = serde_json::from_slice(&response.body).map_err(|error| {
            CoreError::DeviceUnavailable(format!("WDA session response is not JSON: {error}"))
        })?;
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("value")
                    .and_then(Value::as_object)
                    .and_then(|value| value.get("sessionId").or_else(|| value.get("session_id")))
                    .and_then(Value::as_str)
            })
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CoreError::DeviceUnavailable("WDA did not return a session ID".into()))?
            .to_owned();
        self.session_id = Some(session_id.clone());
        Ok(session_id)
    }

    async fn devicekit_rpc(&self, method: &str, params: Value) -> CoreResult<UiHttpResponse> {
        self.devicekit_http(
            Method::POST,
            "/rpc",
            Some(json_bytes(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": 1,
            }))?),
        )
        .await
    }

    async fn wda_json(
        &self,
        method: Method,
        path: &str,
        body: Value,
    ) -> CoreResult<UiHttpResponse> {
        self.wda_http(method, path, Some(json_bytes(body)?)).await
    }

    async fn devicekit_http(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> CoreResult<UiHttpResponse> {
        self.do_http(method, &self.devicekit_url, path, body).await
    }

    async fn wda_http(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> CoreResult<UiHttpResponse> {
        self.do_http(method, &self.wda_url, path, body).await
    }

    async fn do_http(
        &self,
        method: Method,
        base_url: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> CoreResult<UiHttpResponse> {
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        let url = format!("{base_url}{path}");
        let mut request = self
            .http
            .request(method, &url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = request.send().await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("UI backend request {url:?} failed: {error}"))
        })?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("read UI backend response: {error}"))
        })?;
        if !status.is_success() {
            return Err(CoreError::DeviceUnavailable(format!(
                "UI backend returned HTTP {status}: {}",
                response_excerpt(&body)
            )));
        }
        Ok(UiHttpResponse {
            status,
            content_type,
            body: body.to_vec(),
        })
    }
}

async fn healthy(http: &Client, base_url: &str, path: &str) -> bool {
    http.get(format!("{base_url}{path}"))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

fn parse_arguments(arguments: &[String]) -> CoreResult<ParsedUiArgs> {
    if arguments.first().map(String::as_str) != Some("ui") {
        return Err(CoreError::InvalidArgument(
            "UI invocation must start with ui".into(),
        ));
    }
    let action = arguments
        .get(1)
        .filter(|value| !value.starts_with('-'))
        .cloned()
        .ok_or_else(|| CoreError::InvalidArgument("ui requires a subcommand".into()))?;
    let mut positionals = Vec::new();
    let mut options = BTreeMap::new();
    let mut index = 2;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !argument.starts_with("--") {
            positionals.push(argument.clone());
            index += 1;
            continue;
        }
        let (name, value, consumed) = if let Some((name, value)) = argument.split_once('=') {
            (name, value.to_owned(), 1)
        } else {
            let value = arguments.get(index + 1).ok_or_else(|| {
                CoreError::InvalidArgument(format!("{argument} requires a value"))
            })?;
            if value.starts_with("--") {
                return Err(CoreError::InvalidArgument(format!(
                    "{argument} requires a value"
                )));
            }
            (argument.as_str(), value.clone(), 2)
        };
        if !VALUE_OPTIONS.contains(&name) {
            return Err(CoreError::InvalidArgument(format!(
                "unknown ui option {name:?}"
            )));
        }
        if value.is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "{name} requires a non-empty value"
            )));
        }
        if options.insert(name.into(), value).is_some() {
            return Err(CoreError::InvalidArgument(format!(
                "ui option {name:?} was provided more than once"
            )));
        }
        index += consumed;
    }
    Ok(ParsedUiArgs {
        action,
        positionals,
        options,
    })
}

fn validate_options(arguments: &ParsedUiArgs, action_options: &[&str]) -> CoreResult<()> {
    for option in arguments.options.keys() {
        if !COMMON_OPTIONS.contains(&option.as_str()) && !action_options.contains(&option.as_str())
        {
            return Err(CoreError::InvalidArgument(format!(
                "ui {} does not accept {option}",
                arguments.action
            )));
        }
    }
    Ok(())
}

fn require_positionals(arguments: &ParsedUiArgs, expected: usize, usage: &str) -> CoreResult<()> {
    if arguments.positionals.len() != expected {
        return Err(CoreError::InvalidArgument(format!(
            "{usage} expects {expected} positional value(s), got {}",
            arguments.positionals.len()
        )));
    }
    Ok(())
}

fn required_option<'a>(arguments: &'a ParsedUiArgs, name: &str) -> CoreResult<&'a str> {
    arguments
        .options
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| {
            CoreError::InvalidArgument(format!("ui {} requires {name}", arguments.action))
        })
}

fn integer_option(arguments: &ParsedUiArgs, name: &str) -> CoreResult<i64> {
    let value = required_option(arguments, name)?;
    value.parse::<i64>().map_err(|_| {
        CoreError::InvalidArgument(format!("{name} must be an integer, got {value:?}"))
    })
}

fn optional_nonnegative_float(
    arguments: &ParsedUiArgs,
    name: &str,
    default: f64,
) -> CoreResult<f64> {
    let Some(value) = arguments.options.get(name) else {
        return Ok(default);
    };
    let parsed = value.parse::<f64>().map_err(|_| {
        CoreError::InvalidArgument(format!("{name} must be a number, got {value:?}"))
    })?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(CoreError::InvalidArgument(format!(
            "{name} must be a finite non-negative number"
        )));
    }
    Ok(parsed)
}

fn reject_present(arguments: &ParsedUiArgs, names: &[&str], context: &str) -> CoreResult<()> {
    if let Some(name) = names
        .iter()
        .find(|name| arguments.options.contains_key(**name))
    {
        return Err(CoreError::InvalidArgument(format!(
            "{context} does not accept {name}"
        )));
    }
    Ok(())
}

fn raw_option_or_file(
    arguments: &ParsedUiArgs,
    invocation: &CommandInvocation,
    inline_name: &str,
    file_name: &str,
) -> CoreResult<Option<Vec<u8>>> {
    match (
        arguments.options.get(inline_name),
        arguments.options.get(file_name),
    ) {
        (Some(_), Some(_)) => Err(CoreError::InvalidArgument(format!(
            "use only one of {inline_name} and {file_name}"
        ))),
        (Some(value), None) => Ok(Some(value.as_bytes().to_vec())),
        (None, Some(path)) => {
            let path = resolve_client_path(invocation, path);
            std::fs::read(&path).map(Some).map_err(|error| {
                CoreError::InvalidArgument(format!("read {}: {error}", path.display()))
            })
        }
        (None, None) => Ok(None),
    }
}

fn json_option_or_file(
    arguments: &ParsedUiArgs,
    invocation: &CommandInvocation,
    inline_name: &str,
    file_name: &str,
) -> CoreResult<Option<Value>> {
    raw_option_or_file(arguments, invocation, inline_name, file_name)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| {
                CoreError::InvalidArgument(format!(
                    "{inline_name}/{file_name} is not valid JSON: {error}"
                ))
            })
        })
        .transpose()
}

fn normalized_base_url(value: String, option: &str) -> CoreResult<String> {
    let url = reqwest::Url::parse(&value).map_err(|error| {
        CoreError::InvalidArgument(format!("{option} is not a valid URL: {error}"))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(CoreError::InvalidArgument(format!(
            "{option} must be an absolute http(s) URL"
        )));
    }
    Ok(value.trim_end_matches('/').to_owned())
}

fn json_bytes(value: Value) -> CoreResult<Vec<u8>> {
    serde_json::to_vec(&value)
        .map_err(|error| CoreError::Backend(format!("encode UI request JSON: {error}")))
}

fn response_value(response: UiHttpResponse) -> CoreResult<Value> {
    if response.body.is_empty() {
        return Ok(json!({
            "statusCode": response.status.as_u16(),
            "contentType": response.content_type,
        }));
    }
    match serde_json::from_slice(&response.body) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(
            String::from_utf8_lossy(&response.body).into_owned(),
        )),
    }
}

fn find_nested_string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    if let Some(value) = value.as_str() {
        return Some(value);
    }
    let object = value.as_object()?;
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find_map(|value| find_nested_string(value, keys))
}

fn resolve_client_path(invocation: &CommandInvocation, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_owned()
    } else if let Some(cwd) = &invocation.cwd {
        PathBuf::from(cwd).join(path)
    } else {
        path.to_owned()
    }
}

async fn write_output(path: &Path, bytes: &[u8]) -> CoreResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

fn response_excerpt(body: &[u8]) -> String {
    const LIMIT: usize = 512;
    let body = &body[..body.len().min(LIMIT)];
    String::from_utf8_lossy(body).replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    fn invocation(arguments: &[&str], cwd: Option<PathBuf>) -> CommandInvocation {
        CommandInvocation {
            argv: arguments.iter().map(|value| (*value).into()).collect(),
            cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
            udid: None,
            output: Default::default(),
        }
    }

    async fn serve_once(
        body: &'static str,
    ) -> (
        String,
        oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .or_else(|| line.strip_prefix("Content-Length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            }
            let _ = sender.send(String::from_utf8_lossy(&request).into_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), receiver, task)
    }

    async fn serve_bytes_once(
        body: Vec<u8>,
        path: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if count == 0 || request.windows(4).any(|value| value == b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&request).starts_with(&format!("GET {path} ")));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        (format!("http://{address}{path}"), task)
    }

    #[tokio::test]
    async fn wda_status_preserves_json_response() {
        let (url, request, server) = serve_once(r#"{"value":{"ready":true}}"#).await;
        let result = execute(&invocation(
            &["ui", "status", "--driver=wda", &format!("--wda-url={url}")],
            None,
        ))
        .await
        .unwrap();
        assert_eq!(result["value"]["ready"], true);
        assert!(request.await.unwrap().starts_with("GET /status HTTP/1.1"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn devicekit_tap_uses_json_rpc_shape() {
        let (url, request, server) = serve_once(r#"{"jsonrpc":"2.0","result":true,"id":1}"#).await;
        let result = execute(&invocation(
            &[
                "ui",
                "tap",
                "--driver=devicekit",
                &format!("--devicekit-url={url}"),
                "--x=12",
                "--y=34",
            ],
            None,
        ))
        .await
        .unwrap();
        assert_eq!(result["result"], true);
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /rpc HTTP/1.1"));
        assert!(request.contains(r#""method":"device.io.tap""#));
        assert!(request.contains(r#""x":12"#));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wda_tap_reuses_explicit_session() {
        let (url, request, server) = serve_once(r#"{"value":null}"#).await;
        execute(&invocation(
            &[
                "ui",
                "tap",
                "--driver=wda",
                &format!("--wda-url={url}"),
                "--session-id=session-1",
                "--x=5",
                "--y=7",
            ],
            None,
        ))
        .await
        .unwrap();
        assert!(
            request
                .await
                .unwrap()
                .starts_with("POST /session/session-1/wda/tap/5/7 HTTP/1.1")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn screenshot_decodes_base64_to_client_path() {
        let (url, request, server) = serve_once(r#"{"value":"iVBORw0KGgo="}"#).await;
        let directory = tempfile::tempdir().unwrap();
        let result = execute(&invocation(
            &[
                "ui",
                "screenshot",
                "--driver=wda",
                &format!("--wda-url={url}"),
                "--session-id=session-1",
                "--output=capture.png",
            ],
            Some(directory.path().to_owned()),
        ))
        .await
        .unwrap();
        assert_eq!(result["bytes"], 8);
        assert_eq!(
            std::fs::read(directory.path().join("capture.png")).unwrap(),
            b"\x89PNG\r\n\x1a\n"
        );
        assert!(
            request
                .await
                .unwrap()
                .starts_with("GET /session/session-1/screenshot HTTP/1.1")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn artifact_download_streams_and_extracts_wda_zip() {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file(
                "WebDriverAgentRunner.app/Info.plist",
                SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"test plist").unwrap();
        let archive = archive.finish().unwrap().into_inner();
        let (url, server) = serve_bytes_once(archive.clone(), "/Runner.zip").await;
        let output = tempfile::tempdir().unwrap();
        let client = Client::builder().build().unwrap();

        let result = download_target_from_url(&client, UiArtifactTarget::Wda, output.path(), &url)
            .await
            .unwrap();
        assert_eq!(result["sizeBytes"], archive.len() as u64);
        assert_eq!(result["artifactType"], "zip");
        assert!(Path::new(result["artifactPath"].as_str().unwrap()).is_file());
        assert!(Path::new(result["appPath"].as_str().unwrap()).is_dir());
        server.await.unwrap();
    }

    #[test]
    fn parser_rejects_persistent_and_ambiguous_inputs() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for invocation in [
            invocation(&["ui", "stream", "mjpeg", "--fps=30", "--quality=80"], None),
            invocation(
                &[
                    "ui",
                    "install",
                    "wda",
                    "--p12file=identity.p12",
                    "--profile=runner.mobileprovision",
                ],
                None,
            ),
        ] {
            assert!(matches!(
                runtime.block_on(execute(&invocation)),
                Err(CoreError::NotImplemented(_))
            ));
        }

        let stdout_screenshot =
            invocation(&["ui", "screenshot", "--driver=wda", "--output=-"], None);
        assert!(matches!(
            runtime.block_on(execute(&stdout_screenshot)),
            Err(CoreError::NotImplemented(_))
        ));

        let parsed = parse_arguments(&[
            "ui".into(),
            "api".into(),
            "--body={}".into(),
            "--body-file=request.json".into(),
        ])
        .unwrap();
        assert!(
            raw_option_or_file(
                &parsed,
                &invocation(&["ui", "api"], None),
                "--body",
                "--body-file"
            )
            .is_err()
        );
    }
}
