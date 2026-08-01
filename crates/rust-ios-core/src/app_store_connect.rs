use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use apple_codesign::cryptography::InMemoryPrivateKey;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use p12_keystore::{Certificate, KeyStore, KeyStoreEntry, PrivateKeyChain};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use reqwest::{Client, Method, StatusCode, Url};
use rsa::RsaPrivateKey;
use rsa::pkcs8::EncodePrivateKey;
use rust_ios_protocol::{CommandInvocation, CommandResult};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sha1::{Digest, Sha1};
use uuid::Uuid;
use x509_certificate::{CapturedX509Certificate, Sign, X509CertificateBuilder};

use crate::{CoreError, CoreResult};

const DEFAULT_API_BASE_URL: &str = "https://api.appstoreconnect.apple.com";
const API_TIMEOUT: Duration = Duration::from_secs(60);
const JWT_LIFETIME_SECS: u64 = 20 * 60;

const CREDENTIAL_OPTIONS: &[&str] = &["--asc-key-id", "--asc-issuer-id", "--asc-private-key"];
const CERTIFICATE_OPTIONS: &[&str] = &[
    "--asc-key-id",
    "--asc-issuer-id",
    "--asc-private-key",
    "--p12-output",
    "--p12password",
];
const PROVISION_OPTIONS: &[&str] = &[
    "--asc-key-id",
    "--asc-issuer-id",
    "--asc-private-key",
    "--bundleid",
    "--bundle-name",
    "--profile-name",
    "--device-name",
    "--p12-output",
    "--p12password",
    "--profile-output",
    "--certificate-id",
];

struct Credentials {
    key_id: String,
    issuer_id: String,
    signing_key: SigningKey,
}

struct GeneratedIdentity {
    private_key_pkcs8: Vec<u8>,
    signer: InMemoryPrivateKey,
    csr_pem: String,
}

#[derive(Default)]
struct ParsedOptions {
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
struct ResourceResponse {
    data: Resource,
}

#[derive(Debug, Deserialize)]
struct ResourceListResponse {
    #[serde(default)]
    data: Vec<Resource>,
}

#[derive(Debug, Deserialize)]
struct Resource {
    id: String,
    #[serde(default)]
    attributes: Map<String, Value>,
}

struct ApiClient {
    http: Client,
    base_url: Url,
    credentials: Credentials,
    token: Option<(String, u64)>,
}

pub(crate) async fn execute(invocation: &CommandInvocation) -> CoreResult<CommandResult> {
    match invocation.argv.get(1).map(String::as_str) {
        Some("certificate") => certificate_command(invocation).await,
        Some("provision") => provision_command(invocation).await,
        Some(action) => Err(CoreError::InvalidArgument(format!(
            "unknown sign appstoreconnect action {action:?}"
        ))),
        None => Err(CoreError::InvalidArgument(
            "sign appstoreconnect requires certificate or provision".into(),
        )),
    }
}

async fn certificate_command(invocation: &CommandInvocation) -> CoreResult<CommandResult> {
    require_appstoreconnect_service(invocation)?;
    let options = parse_options(
        &invocation.argv[3..],
        CERTIFICATE_OPTIONS,
        &["--revoke-existing"],
    )?;
    let credentials = load_credentials(invocation, &options)?;
    let p12_path = option_path(invocation, &options, "--p12-output")
        .unwrap_or_else(|| resolve_client_path(invocation, "identity.p12"));
    prepare_output_path(&p12_path, "P12 output")?;
    let password = secret_option_or_env(&options, "--p12password", "P12_PASSWORD");
    let identity = tokio::task::spawn_blocking(|| generate_identity("rust-ios signing identity"))
        .await
        .map_err(|error| {
            CoreError::Backend(format!("signing identity worker failed: {error}"))
        })??;
    let mut client = ApiClient::new(credentials)?;
    let revoked = if options.flags.contains("--revoke-existing") {
        client.revoke_all_development_certificates().await?
    } else {
        Vec::new()
    };
    let (certificate_id, certificate_der) = client.create_certificate(&identity.csr_pem).await?;
    let output = p12_path.clone();
    tokio::task::spawn_blocking(move || {
        let p12 = encode_p12(&identity, &certificate_der, &password)?;
        atomic_write(&output, &p12, 0o600)
    })
    .await
    .map_err(|error| CoreError::Backend(format!("P12 output worker failed: {error}")))??;

    Ok(CommandResult {
        exit_code: 0,
        value: Some(json!({
            "p12Path": p12_path,
            "certificateID": certificate_id,
            "revokedCertificateIDs": revoked,
        })),
        text: None,
        warnings: Vec::new(),
    })
}

async fn provision_command(invocation: &CommandInvocation) -> CoreResult<CommandResult> {
    require_appstoreconnect_service(invocation)?;
    let options = parse_options(
        &invocation.argv[3..],
        PROVISION_OPTIONS,
        &["--revoke-existing"],
    )?;
    let bundle_id = required_option(&options, "--bundleid")?;
    let udid = invocation
        .udid
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CoreError::InvalidArgument(
                "sign provision appstoreconnect requires --udid or GO_IOS_UDID".into(),
            )
        })?;
    let certificate_id = options.values.get("--certificate-id").cloned();
    if certificate_id.is_some() {
        if options.flags.contains("--revoke-existing") {
            return Err(CoreError::InvalidArgument(
                "--revoke-existing cannot be combined with --certificate-id".into(),
            ));
        }
        if options.values.contains_key("--p12-output") {
            return Err(CoreError::InvalidArgument(
                "--p12-output cannot be combined with --certificate-id because no P12 is created"
                    .into(),
            ));
        }
    }

    let credentials = load_credentials(invocation, &options)?;
    let profile_path = option_path(invocation, &options, "--profile-output").unwrap_or_else(|| {
        resolve_client_path(invocation, &format!("{bundle_id}.mobileprovision"))
    });
    prepare_output_path(&profile_path, "provisioning profile output")?;
    let p12_path = if certificate_id.is_none() {
        let path = option_path(invocation, &options, "--p12-output")
            .unwrap_or_else(|| resolve_client_path(invocation, &format!("{bundle_id}.p12")));
        prepare_output_path(&path, "P12 output")?;
        Some(path)
    } else {
        None
    };
    let generated_identity = if certificate_id.is_none() {
        let common_name = format!("rust-ios {bundle_id}");
        Some(
            tokio::task::spawn_blocking(move || generate_identity(&common_name))
                .await
                .map_err(|error| {
                    CoreError::Backend(format!("signing identity worker failed: {error}"))
                })??,
        )
    } else {
        None
    };

    let mut client = ApiClient::new(credentials)?;
    let bundle_resource_id = client
        .ensure_bundle_id(
            bundle_id,
            options.values.get("--bundle-name").map(String::as_str),
        )
        .await?;
    let device_resource_id = client
        .ensure_device(
            udid,
            options.values.get("--device-name").map(String::as_str),
        )
        .await?;

    let password = secret_option_or_env(&options, "--p12password", "P12_PASSWORD");
    let mut revoked = Vec::new();
    let certificate_id = match certificate_id {
        Some(id) => id,
        None => {
            let identity =
                generated_identity.expect("signing identity exists when creating a certificate");
            if options.flags.contains("--revoke-existing") {
                revoked = client.revoke_all_development_certificates().await?;
            }
            let (id, certificate_der) = client.create_certificate(&identity.csr_pem).await?;
            let output = p12_path
                .clone()
                .expect("P12 path exists when creating a certificate");
            tokio::task::spawn_blocking(move || {
                let p12 = encode_p12(&identity, &certificate_der, &password)?;
                atomic_write(&output, &p12, 0o600)
            })
            .await
            .map_err(|error| CoreError::Backend(format!("P12 output worker failed: {error}")))??;
            id
        }
    };

    let profile_name = options
        .values
        .get("--profile-name")
        .cloned()
        .unwrap_or_else(|| {
            format!(
                "rust-ios {bundle_id} {}",
                unix_time_secs().unwrap_or_default()
            )
        });
    let profile = client
        .create_development_profile(
            &profile_name,
            &bundle_resource_id,
            &certificate_id,
            &device_resource_id,
        )
        .await?;
    let output = profile_path.clone();
    tokio::task::spawn_blocking(move || atomic_write(&output, &profile, 0o644))
        .await
        .map_err(|error| CoreError::Backend(format!("profile output worker failed: {error}")))??;

    Ok(CommandResult {
        exit_code: 0,
        value: Some(json!({
            "bundleID": bundle_id,
            "bundleResourceID": bundle_resource_id,
            "deviceResourceID": device_resource_id,
            "certificateID": certificate_id,
            "p12Path": p12_path,
            "profilePath": profile_path,
            "revokedCertificateIDs": revoked,
        })),
        text: None,
        warnings: Vec::new(),
    })
}

fn require_appstoreconnect_service(invocation: &CommandInvocation) -> CoreResult<()> {
    if invocation.argv.get(2).map(String::as_str) == Some("appstoreconnect") {
        Ok(())
    } else {
        Err(CoreError::InvalidArgument(
            "sign certificate/provision requires the appstoreconnect service".into(),
        ))
    }
}

fn parse_options(
    arguments: &[String],
    allowed_values: &[&str],
    allowed_flags: &[&str],
) -> CoreResult<ParsedOptions> {
    let mut parsed = ParsedOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        let (name, inline_value) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        if allowed_flags.contains(&name) {
            if inline_value.is_some() {
                return Err(CoreError::InvalidArgument(format!(
                    "flag {name} does not accept a value"
                )));
            }
            if !parsed.flags.insert(name.to_owned()) {
                return Err(CoreError::InvalidArgument(format!(
                    "flag {name} was provided more than once"
                )));
            }
            index += 1;
            continue;
        }
        if !allowed_values.contains(&name) {
            return Err(CoreError::InvalidArgument(if name.starts_with('-') {
                format!("unknown App Store Connect option {name:?}")
            } else {
                format!("unexpected App Store Connect positional value {argument:?}")
            }));
        }
        let value = if let Some(value) = inline_value {
            value.to_owned()
        } else {
            index += 1;
            arguments
                .get(index)
                .filter(|value| !value.starts_with('-'))
                .cloned()
                .ok_or_else(|| CoreError::InvalidArgument(format!("option {name} needs a value")))?
        };
        if value.trim().is_empty() {
            return Err(CoreError::InvalidArgument(format!(
                "option {name} cannot be empty"
            )));
        }
        if parsed.values.insert(name.to_owned(), value).is_some() {
            return Err(CoreError::InvalidArgument(format!(
                "option {name} was provided more than once"
            )));
        }
        index += 1;
    }
    Ok(parsed)
}

fn required_option<'a>(options: &'a ParsedOptions, name: &str) -> CoreResult<&'a str> {
    options
        .values
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| CoreError::InvalidArgument(format!("{name} is required")))
}

fn option_or_env(options: &ParsedOptions, name: &str, environment: &str) -> Option<String> {
    options
        .values
        .get(name)
        .cloned()
        .or_else(|| std::env::var(environment).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn secret_option_or_env(options: &ParsedOptions, name: &str, environment: &str) -> String {
    options
        .values
        .get(name)
        .cloned()
        .or_else(|| std::env::var(environment).ok())
        .unwrap_or_default()
}

fn load_credentials(
    invocation: &CommandInvocation,
    options: &ParsedOptions,
) -> CoreResult<Credentials> {
    for name in options.values.keys() {
        if name.starts_with("--asc-") && !CREDENTIAL_OPTIONS.contains(&name.as_str()) {
            return Err(CoreError::InvalidArgument(format!(
                "unknown App Store Connect credential {name}"
            )));
        }
    }
    let key_id = option_or_env(options, "--asc-key-id", "GO_IOS_ASC_KEY_ID").ok_or_else(|| {
        CoreError::InvalidArgument("--asc-key-id is required or set GO_IOS_ASC_KEY_ID".into())
    })?;
    let issuer_id =
        option_or_env(options, "--asc-issuer-id", "GO_IOS_ASC_ISSUER_ID").ok_or_else(|| {
            CoreError::InvalidArgument(
                "--asc-issuer-id is required or set GO_IOS_ASC_ISSUER_ID".into(),
            )
        })?;
    let private_key_path = option_or_env(options, "--asc-private-key", "GO_IOS_ASC_PRIVATE_KEY")
        .ok_or_else(|| {
            CoreError::InvalidArgument(
                "--asc-private-key is required or set GO_IOS_ASC_PRIVATE_KEY".into(),
            )
        })?;
    let private_key_path = resolve_client_path(invocation, &private_key_path);
    let private_key = fs::read_to_string(&private_key_path).map_err(|error| {
        CoreError::InvalidArgument(format!(
            "read App Store Connect private key {}: {error}",
            private_key_path.display()
        ))
    })?;
    let signing_key = SigningKey::from_pkcs8_pem(&private_key).map_err(|error| {
        CoreError::InvalidArgument(format!(
            "parse App Store Connect P-256 private key {}: {error}",
            private_key_path.display()
        ))
    })?;
    Ok(Credentials {
        key_id,
        issuer_id,
        signing_key,
    })
}

fn option_path(
    invocation: &CommandInvocation,
    options: &ParsedOptions,
    name: &str,
) -> Option<PathBuf> {
    options
        .values
        .get(name)
        .map(|value| resolve_client_path(invocation, value))
}

fn resolve_client_path(invocation: &CommandInvocation, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        invocation
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(path)
    }
}

fn prepare_output_path(path: &Path, label: &str) -> CoreResult<()> {
    if path.as_os_str().is_empty() {
        return Err(CoreError::InvalidArgument(format!(
            "{label} path cannot be empty"
        )));
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(CoreError::PermissionDenied(format!(
                "refusing to replace {label} symlink {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            return Err(CoreError::InvalidArgument(format!(
                "{label} path is a directory: {}",
                path.display()
            )));
        }
    }
    let parent = path.parent().ok_or_else(|| {
        CoreError::InvalidArgument(format!("{label} has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent)?;
    Ok(())
}

fn generate_identity(common_name: &str) -> CoreResult<GeneratedIdentity> {
    let mut rng = rand_08::rngs::OsRng;
    let rsa_key = RsaPrivateKey::new(&mut rng, 2048)
        .map_err(|error| CoreError::Backend(format!("generate RSA signing key: {error}")))?;
    let private_key = rsa_key
        .to_pkcs8_der()
        .map_err(|error| CoreError::Backend(format!("encode RSA PKCS#8 key: {error}")))?;
    let private_key_pkcs8 = private_key.as_bytes().to_vec();
    let signer = InMemoryPrivateKey::from_pkcs8_der(&private_key_pkcs8)
        .map_err(|error| CoreError::Backend(format!("load generated RSA key: {error}")))?;
    let mut builder = X509CertificateBuilder::default();
    builder
        .subject()
        .append_common_name_utf8_string(common_name)
        .map_err(|error| CoreError::Backend(format!("build CSR subject: {error:?}")))?;
    let request = builder
        .create_certificate_signing_request(&signer)
        .map_err(|error| CoreError::Backend(format!("create signing CSR: {error}")))?;
    let csr_pem = request
        .encode_pem()
        .map_err(|error| CoreError::Backend(format!("encode signing CSR PEM: {error}")))?;
    Ok(GeneratedIdentity {
        private_key_pkcs8,
        signer,
        csr_pem,
    })
}

fn encode_p12(
    identity: &GeneratedIdentity,
    certificate_der: &[u8],
    password: &str,
) -> CoreResult<Vec<u8>> {
    let captured = CapturedX509Certificate::from_der(certificate_der.to_vec())
        .map_err(|error| CoreError::Backend(format!("parse Apple certificate: {error}")))?;
    if captured.public_key_data() != identity.signer.public_key_data() {
        return Err(CoreError::Backend(
            "Apple certificate public key does not match generated CSR private key".into(),
        ));
    }
    let certificate = Certificate::from_der(certificate_der)
        .map_err(|error| CoreError::Backend(format!("parse certificate for P12: {error}")))?;
    let local_key_id = Sha1::digest(certificate_der).to_vec();
    let mut store = KeyStore::new();
    store.add_entry(
        "rust-ios signing identity",
        KeyStoreEntry::PrivateKeyChain(PrivateKeyChain::new(
            &identity.private_key_pkcs8,
            local_key_id,
            [certificate],
        )),
    );
    store
        .writer(password)
        .write()
        .map_err(|error| CoreError::Backend(format!("encode signing P12: {error}")))
}

fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> CoreResult<()> {
    prepare_output_path(path, "output")?;
    let parent = path.parent().expect("output parent was validated");
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("app-store-connect-output");
    let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temporary, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(CoreError::Io)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: 两个 UTF-16 路径均以 NUL 结尾，并在调用期间保持有效。
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

impl ApiClient {
    fn new(credentials: Credentials) -> CoreResult<Self> {
        let base = std::env::var("RUST_IOS_ASC_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_API_BASE_URL.to_owned());
        let base_url = Url::parse(base.trim_end_matches('/')).map_err(|error| {
            CoreError::InvalidArgument(format!("invalid App Store Connect API base URL: {error}"))
        })?;
        if base_url.scheme() != "https" && !base_url.host_str().is_some_and(is_loopback_host) {
            return Err(CoreError::PermissionDenied(
                "App Store Connect API base URL must use HTTPS unless it is loopback".into(),
            ));
        }
        let http = Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .map_err(|error| CoreError::Backend(format!("build HTTP client: {error}")))?;
        Ok(Self {
            http,
            base_url,
            credentials,
            token: None,
        })
    }

    async fn ensure_bundle_id(
        &mut self,
        identifier: &str,
        name: Option<&str>,
    ) -> CoreResult<String> {
        let mut url = self.url("/v1/bundleIds")?;
        url.query_pairs_mut()
            .append_pair("filter[identifier]", identifier)
            .append_pair("filter[platform]", "IOS")
            .append_pair("limit", "1");
        let list: ResourceListResponse = self.send(Method::GET, url, None).await?;
        if let Some(resource) = list.data.into_iter().next() {
            return Ok(resource.id);
        }
        let response: ResourceResponse = self
            .send(
                Method::POST,
                self.url("/v1/bundleIds")?,
                Some(json!({
                    "data": {
                        "type": "bundleIds",
                        "attributes": {
                            "identifier": identifier,
                            "name": name.filter(|value| !value.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| default_bundle_name(identifier)),
                            "platform": "IOS"
                        }
                    }
                })),
            )
            .await?;
        Ok(response.data.id)
    }

    async fn ensure_device(&mut self, udid: &str, name: Option<&str>) -> CoreResult<String> {
        let mut url = self.url("/v1/devices")?;
        url.query_pairs_mut()
            .append_pair("filter[udid]", udid)
            .append_pair("limit", "1");
        let list: ResourceListResponse = self.send(Method::GET, url, None).await?;
        if let Some(resource) = list.data.into_iter().next() {
            return Ok(resource.id);
        }
        let response: ResourceResponse = self
            .send(
                Method::POST,
                self.url("/v1/devices")?,
                Some(json!({
                    "data": {
                        "type": "devices",
                        "attributes": {
                            "name": name.filter(|value| !value.trim().is_empty()).map(str::to_owned).unwrap_or_else(|| format!("rust-ios {udid}")),
                            "platform": "IOS",
                            "udid": udid
                        }
                    }
                })),
            )
            .await?;
        Ok(response.data.id)
    }

    async fn create_certificate(&mut self, csr_pem: &str) -> CoreResult<(String, Vec<u8>)> {
        let response: ResourceResponse = self
            .send(
                Method::POST,
                self.url("/v1/certificates")?,
                Some(json!({
                    "data": {
                        "type": "certificates",
                        "attributes": {
                            "certificateType": "IOS_DEVELOPMENT",
                            "csrContent": csr_pem
                        }
                    }
                })),
            )
            .await?;
        let content = response
            .data
            .attributes
            .get("certificateContent")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CoreError::Backend(
                    "App Store Connect certificate response has no certificateContent".into(),
                )
            })?;
        let certificate = BASE64.decode(content).map_err(|error| {
            CoreError::Backend(format!("decode Apple certificateContent: {error}"))
        })?;
        Ok((response.data.id, certificate))
    }

    async fn revoke_all_development_certificates(&mut self) -> CoreResult<Vec<String>> {
        let mut url = self.url("/v1/certificates")?;
        url.query_pairs_mut()
            .append_pair("filter[certificateType]", "IOS_DEVELOPMENT")
            .append_pair("limit", "200");
        let list: ResourceListResponse = self.send(Method::GET, url, None).await?;
        let mut revoked = Vec::with_capacity(list.data.len());
        for certificate in list.data {
            let id = certificate.id;
            let url = self.resource_url("/v1/certificates/", &id)?;
            self.send_empty(Method::DELETE, url).await?;
            revoked.push(id);
        }
        Ok(revoked)
    }

    async fn create_development_profile(
        &mut self,
        name: &str,
        bundle_id: &str,
        certificate_id: &str,
        device_id: &str,
    ) -> CoreResult<Vec<u8>> {
        let response: ResourceResponse = self
            .send(
                Method::POST,
                self.url("/v1/profiles")?,
                Some(json!({
                    "data": {
                        "type": "profiles",
                        "attributes": {
                            "name": name,
                            "profileType": "IOS_APP_DEVELOPMENT"
                        },
                        "relationships": {
                            "bundleId": {"data": {"type": "bundleIds", "id": bundle_id}},
                            "certificates": {"data": [{"type": "certificates", "id": certificate_id}]},
                            "devices": {"data": [{"type": "devices", "id": device_id}]}
                        }
                    }
                })),
            )
            .await?;
        let content = response
            .data
            .attributes
            .get("profileContent")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                CoreError::Backend(
                    "App Store Connect profile response has no profileContent".into(),
                )
            })?;
        BASE64
            .decode(content)
            .map_err(|error| CoreError::Backend(format!("decode Apple profileContent: {error}")))
    }

    async fn send<T: DeserializeOwned>(
        &mut self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> CoreResult<T> {
        let response = self.send_request(method, url, body).await?;
        response.json::<T>().await.map_err(|error| {
            CoreError::Backend(format!("decode App Store Connect JSON response: {error}"))
        })
    }

    async fn send_empty(&mut self, method: Method, url: Url) -> CoreResult<()> {
        self.send_request(method, url, None).await?;
        Ok(())
    }

    async fn send_request(
        &mut self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> CoreResult<reqwest::Response> {
        let token = self.bearer_token()?;
        let mut request = self
            .http
            .request(method.clone(), url.clone())
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                CoreError::Timeout(format!("App Store Connect {method} {}", url.path()))
            } else {
                CoreError::Backend(format!(
                    "App Store Connect {method} {} failed: {error}",
                    url.path()
                ))
            }
        })?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| CoreError::Backend(format!("read Apple API error body: {error}")))?;
        let message = format!(
            "App Store Connect {method} {} returned {status}: {}",
            url.path(),
            summarize_apple_error(&body)
        );
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            Err(CoreError::PermissionDenied(message))
        } else if status.is_client_error() {
            Err(CoreError::InvalidArgument(message))
        } else {
            Err(CoreError::Backend(message))
        }
    }

    fn bearer_token(&mut self) -> CoreResult<String> {
        let now = unix_time_secs()?;
        if let Some((token, expiry)) = &self.token
            && now.saturating_add(60) < *expiry
        {
            return Ok(token.clone());
        }
        let (token, expiry) = make_jwt(&self.credentials, now)?;
        self.token = Some((token.clone(), expiry));
        Ok(token)
    }

    fn url(&self, path: &str) -> CoreResult<Url> {
        self.base_url.join(path).map_err(|error| {
            CoreError::InvalidArgument(format!("build App Store Connect URL: {error}"))
        })
    }

    fn resource_url(&self, collection: &str, id: &str) -> CoreResult<Url> {
        let mut url = self.url(collection)?;
        url.path_segments_mut()
            .map_err(|_| {
                CoreError::InvalidArgument(
                    "App Store Connect API base URL cannot contain an opaque path".into(),
                )
            })?
            .pop_if_empty()
            .push(id);
        Ok(url)
    }
}

fn make_jwt(credentials: &Credentials, now: u64) -> CoreResult<(String, u64)> {
    let expiry = now.saturating_add(JWT_LIFETIME_SECS);
    let header = serde_json::to_vec(&json!({
        "alg": "ES256",
        "kid": credentials.key_id,
        "typ": "JWT"
    }))
    .map_err(|error| CoreError::Backend(format!("encode JWT header: {error}")))?;
    let claims = serde_json::to_vec(&json!({
        "iss": credentials.issuer_id,
        "iat": now,
        "exp": expiry,
        "aud": "appstoreconnect-v1"
    }))
    .map_err(|error| CoreError::Backend(format!("encode JWT claims: {error}")))?;
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let signature: Signature = credentials.signing_key.sign(signing_input.as_bytes());
    Ok((
        format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ),
        expiry,
    ))
}

fn summarize_apple_error(bytes: &[u8]) -> String {
    let value = serde_json::from_slice::<Value>(bytes).ok();
    if let Some(errors) = value
        .as_ref()
        .and_then(|value| value.get("errors"))
        .and_then(Value::as_array)
    {
        let messages = errors
            .iter()
            .filter_map(|error| {
                let title = error.get("title").and_then(Value::as_str).unwrap_or("");
                let detail = error.get("detail").and_then(Value::as_str).unwrap_or("");
                let message = format!("{title}: {detail}").trim().to_owned();
                (!message.is_empty()).then(|| truncate(&message, 512))
            })
            .collect::<Vec<_>>();
        if !messages.is_empty() {
            return messages.join("; ");
        }
    }
    truncate(&String::from_utf8_lossy(bytes), 2048)
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

fn default_bundle_name(identifier: &str) -> String {
    let name = identifier
        .replace(['.', '-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        "rust ios app".into()
    } else {
        name
    }
}

fn unix_time_secs() -> CoreResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| CoreError::Backend(format!("system clock is before Unix epoch: {error}")))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use p256::ecdsa::signature::Verifier as _;
    use p256::pkcs8::EncodePrivateKey as _;
    use x509_certificate::InMemorySigningKeyPair;

    use super::*;

    #[test]
    fn jwt_is_es256_and_verifiable() {
        let signing_key = SigningKey::random(&mut rand_08::rngs::OsRng);
        let credentials = Credentials {
            key_id: "KEY123".into(),
            issuer_id: "issuer-123".into(),
            signing_key: signing_key.clone(),
        };
        let (token, expiry) = make_jwt(&credentials, 1_700_000_000).unwrap();
        assert_eq!(expiry, 1_700_001_200);
        let parts = token.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "KEY123");
        assert_eq!(claims["aud"], "appstoreconnect-v1");
        assert_eq!(claims["exp"], 1_700_001_200_u64);
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        signing_key
            .verifying_key()
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap();

        let pem = signing_key.to_pkcs8_pem(Default::default()).unwrap();
        let reparsed = SigningKey::from_pkcs8_pem(pem.as_str()).unwrap();
        assert_eq!(
            reparsed.verifying_key().to_encoded_point(false),
            signing_key.verifying_key().to_encoded_point(false)
        );
        let client = ApiClient::new(credentials).unwrap();
        assert_eq!(
            client
                .resource_url("/v1/certificates/", "id/with/slash")
                .unwrap()
                .path(),
            "/v1/certificates/id%2Fwith%2Fslash"
        );
    }

    #[test]
    fn generated_identity_builds_matching_p12() {
        let identity = generate_identity("rust-ios test").unwrap();
        assert!(identity.csr_pem.contains("BEGIN CERTIFICATE REQUEST"));
        let pair: InMemorySigningKeyPair = identity.signer.clone().try_into().unwrap();
        let mut builder = X509CertificateBuilder::default();
        builder
            .subject()
            .append_common_name_utf8_string("rust-ios test certificate")
            .unwrap();
        let certificate = builder.create_with_key_pair(&pair).unwrap();
        let p12 = encode_p12(&identity, certificate.constructed_data(), "secret").unwrap();
        let store = KeyStore::from_pkcs12(&p12, "secret").unwrap();
        let (_, chain) = store.private_key_chain().unwrap();
        assert_eq!(chain.chain()[0].as_der(), certificate.constructed_data());
        assert_eq!(chain.key(), identity.private_key_pkcs8);
    }

    #[test]
    fn parser_rejects_ambiguous_or_unsafe_reuse_options() {
        let options = parse_options(
            &[
                "--bundleid=com.example.runner".into(),
                "--certificate-id".into(),
                "cert-1".into(),
            ],
            PROVISION_OPTIONS,
            &["--revoke-existing"],
        )
        .unwrap();
        assert_eq!(options.values["--certificate-id"], "cert-1");
        let password = parse_options(
            &["--p12password= padded secret ".into()],
            CERTIFICATE_OPTIONS,
            &["--revoke-existing"],
        )
        .unwrap();
        assert_eq!(
            secret_option_or_env(&password, "--p12password", "UNSET_TEST_PASSWORD"),
            " padded secret "
        );
        assert!(
            parse_options(
                &["--bundleid=a".into(), "--bundleid=b".into()],
                PROVISION_OPTIONS,
                &["--revoke-existing"]
            )
            .is_err()
        );
        assert!(
            parse_options(
                &["--unknown=value".into()],
                PROVISION_OPTIONS,
                &["--revoke-existing"]
            )
            .is_err()
        );
    }

    #[test]
    fn apple_errors_are_bounded_and_structured() {
        let error = summarize_apple_error(
            br#"{"errors":[{"title":"Conflict","detail":"certificate limit reached"}]}"#,
        );
        assert_eq!(error, "Conflict: certificate limit reached");
        assert!(summarize_apple_error(&vec![b'x'; 4096]).len() <= 2051);
    }
}
