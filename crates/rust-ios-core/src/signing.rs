use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use apple_codesign::cryptography::{InMemoryPrivateKey, PrivateKey};
use apple_codesign::{SettingsScope, SigningSettings, UnifiedSigner};
use cryptographic_message_syntax::SignedData;
use p12_keystore::KeyStore;
use plist::Value;
use rust_ios_protocol::{CommandInvocation, CommandResult};
use serde_json::json;
use x509_certificate::CapturedX509Certificate;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::{CoreError, CoreResult};

const SIGN_APP_VALUE_OPTIONS: &[&str] = &[
    "--path",
    "--p12file",
    "--profile",
    "--p12password",
    "--output",
    "--bundleid",
];

#[derive(Debug, Clone)]
pub(crate) struct SignedApp {
    pub output_path: PathBuf,
    pub bundle_id: String,
}

#[derive(Debug)]
struct ProvisioningProfile {
    entitlements: Value,
    developer_certificates: Vec<Vec<u8>>,
    provisioned_devices: Vec<String>,
    provisions_all_devices: bool,
    expiration_date: Option<SystemTime>,
}

#[derive(Debug)]
struct ParsedCommandOptions {
    values: BTreeMap<String, String>,
}

pub(crate) async fn sign_app_command(invocation: &CommandInvocation) -> CoreResult<CommandResult> {
    let signed = prepare_sign_app(invocation, None).await?;
    Ok(CommandResult {
        exit_code: 0,
        value: Some(json!({
            "appPath": signed.output_path,
            "bundleID": signed.bundle_id,
        })),
        text: None,
        warnings: Vec::new(),
    })
}

pub(crate) async fn prepare_sign_app(
    invocation: &CommandInvocation,
    device_udid: Option<&str>,
) -> CoreResult<SignedApp> {
    if invocation.argv.get(1).map(String::as_str) != Some("app") {
        return Err(CoreError::InvalidArgument(
            "sign requires the app subcommand".into(),
        ));
    }
    let options = parse_sign_app_options(&invocation.argv[2..])?;
    let input = required_path(invocation, &options.values, "--path", "sign app")?;
    let p12 = required_path(invocation, &options.values, "--p12file", "sign app")?;
    let profile = required_path(invocation, &options.values, "--profile", "sign app")?;
    let output = options
        .values
        .get("--output")
        .map(|value| resolve_client_path(invocation, value))
        .unwrap_or_else(|| default_signed_output(&input));
    let bundle_id = options.values.get("--bundleid").cloned();
    let password = options
        .values
        .get("--p12password")
        .cloned()
        .or_else(|| std::env::var("P12_PASSWORD").ok())
        .unwrap_or_default();
    sign_with_files(
        input,
        output,
        p12,
        password,
        profile,
        bundle_id,
        device_udid.map(str::to_owned),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn sign_with_files(
    input_path: PathBuf,
    output_path: PathBuf,
    p12_path: PathBuf,
    p12_password: String,
    profile_path: PathBuf,
    bundle_id: Option<String>,
    device_udid: Option<String>,
) -> CoreResult<SignedApp> {
    tokio::task::spawn_blocking(move || {
        sign_with_files_blocking(
            &input_path,
            &output_path,
            &p12_path,
            &p12_password,
            &profile_path,
            bundle_id.as_deref(),
            device_udid.as_deref(),
        )
    })
    .await
    .map_err(|error| CoreError::Backend(format!("signing worker failed: {error}")))?
}

#[allow(clippy::too_many_arguments)]
fn sign_with_files_blocking(
    input_path: &Path,
    output_path: &Path,
    p12_path: &Path,
    p12_password: &str,
    profile_path: &Path,
    requested_bundle_id: Option<&str>,
    device_udid: Option<&str>,
) -> CoreResult<SignedApp> {
    ensure_existing_path(input_path, "app")?;
    ensure_existing_file(p12_path, "P12")?;
    ensure_existing_file(profile_path, "provisioning profile")?;
    if output_path.as_os_str().is_empty() {
        return Err(CoreError::InvalidArgument(
            "sign app output path cannot be empty".into(),
        ));
    }
    if let Some(bundle_id) = requested_bundle_id
        && bundle_id.trim().is_empty()
    {
        return Err(CoreError::InvalidArgument(
            "sign app bundle ID cannot be empty".into(),
        ));
    }

    let p12_data = fs::read(p12_path)?;
    let profile_data = fs::read(profile_path)?;
    let (certificate, private_key) = load_p12_identity(&p12_data, p12_password)?;
    let profile = parse_provisioning_profile(&profile_data)?;
    validate_profile(&profile, certificate.constructed_data(), device_udid)?;

    if input_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("ipa"))
    {
        let temporary = tempfile::tempdir()?;
        let modes = extract_zip(input_path, temporary.path())?;
        let app_path = find_app_bundle(&temporary.path().join("Payload"))?;
        let bundle_id =
            prepare_app_bundle(&app_path, &profile_data, &profile, requested_bundle_id)?;
        validate_profile_bundle_id(&profile, &bundle_id)?;
        let signer = configured_signer(&certificate, &private_key, &profile.entitlements)?;
        signer
            .sign_path_in_place(&app_path)
            .map_err(|error| CoreError::Backend(format!("sign IPA app bundle: {error}")))?;
        ensure_parent(output_path)?;
        if output_path.exists() {
            fs::remove_file(output_path)?;
        }
        repackage_zip(temporary.path(), output_path, &modes)?;
        return Ok(SignedApp {
            output_path: output_path.to_owned(),
            bundle_id,
        });
    }

    if !input_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        || !input_path.is_dir()
    {
        return Err(CoreError::InvalidArgument(format!(
            "sign app input must be an .app directory or .ipa file: {}",
            input_path.display()
        )));
    }

    let temporary = tempfile::tempdir()?;
    let working_app = temporary.path().join(
        input_path
            .file_name()
            .ok_or_else(|| CoreError::InvalidArgument("app path has no file name".into()))?,
    );
    copy_directory(input_path, &working_app)?;
    let bundle_id = prepare_app_bundle(&working_app, &profile_data, &profile, requested_bundle_id)?;
    validate_profile_bundle_id(&profile, &bundle_id)?;
    let signer = configured_signer(&certificate, &private_key, &profile.entitlements)?;
    ensure_parent(output_path)?;
    if output_path.exists() {
        fs::remove_dir_all(output_path)?;
    }
    signer
        .sign_bundle(&working_app, output_path)
        .map_err(|error| CoreError::Backend(format!("sign app bundle: {error}")))?;
    Ok(SignedApp {
        output_path: output_path.to_owned(),
        bundle_id,
    })
}

fn configured_signer<'a>(
    certificate: &CapturedX509Certificate,
    private_key: &'a apple_codesign::cryptography::InMemoryPrivateKey,
    entitlements: &Value,
) -> CoreResult<UnifiedSigner<'a>> {
    let mut settings = SigningSettings::default();
    settings.set_signing_key(private_key.as_key_info_signer(), certificate.clone());
    let _ = settings.chain_apple_certificates();
    let _ = settings.set_team_id_from_signing_certificate();
    let mut entitlement_xml = Vec::new();
    entitlements
        .to_writer_xml(&mut entitlement_xml)
        .map_err(|error| CoreError::InvalidArgument(format!("encode entitlements: {error}")))?;
    settings
        .set_entitlements_xml(
            SettingsScope::Main,
            String::from_utf8(entitlement_xml).map_err(|error| {
                CoreError::Backend(format!("encode entitlements UTF-8: {error}"))
            })?,
        )
        .map_err(|error| CoreError::InvalidArgument(format!("load entitlements: {error}")))?;
    Ok(UnifiedSigner::new(settings))
}

fn load_p12_identity(
    data: &[u8],
    password: &str,
) -> CoreResult<(CapturedX509Certificate, InMemoryPrivateKey)> {
    let store = KeyStore::from_pkcs12(data, password).map_err(|error| {
        CoreError::InvalidArgument(format!("cannot decode P12 identity: {error}"))
    })?;
    let (_, chain) = store.private_key_chain().ok_or_else(|| {
        CoreError::InvalidArgument("P12 identity has no private key chain".into())
    })?;
    let certificate_der = chain
        .chain()
        .first()
        .ok_or_else(|| CoreError::InvalidArgument("P12 private key has no certificate".into()))?
        .as_der();
    let certificate = CapturedX509Certificate::from_der(certificate_der.to_vec())
        .map_err(|error| CoreError::InvalidArgument(format!("parse P12 certificate: {error}")))?;
    let private_key = match InMemoryPrivateKey::from_pkcs8_der(chain.key()) {
        Ok(key) => key,
        Err(pkcs8_error) => {
            InMemoryPrivateKey::from_pkcs1_der(chain.key()).map_err(|pkcs1_error| {
                CoreError::InvalidArgument(format!(
                    "parse P12 private key as PKCS#8 ({pkcs8_error}) or PKCS#1 ({pkcs1_error})"
                ))
            })?
        }
    };
    Ok((certificate, private_key))
}

fn parse_provisioning_profile(data: &[u8]) -> CoreResult<ProvisioningProfile> {
    let signed = SignedData::parse_ber(data).map_err(|error| {
        CoreError::InvalidArgument(format!("parse provisioning profile CMS: {error}"))
    })?;
    let content = signed.signed_content().ok_or_else(|| {
        CoreError::InvalidArgument("provisioning profile CMS contains no plist payload".into())
    })?;
    let value = Value::from_reader(Cursor::new(content)).map_err(|error| {
        CoreError::InvalidArgument(format!("parse provisioning profile plist: {error}"))
    })?;
    let dictionary = value.as_dictionary().ok_or_else(|| {
        CoreError::InvalidArgument("provisioning profile payload is not a dictionary".into())
    })?;
    let entitlements = dictionary.get("Entitlements").cloned().ok_or_else(|| {
        CoreError::InvalidArgument("provisioning profile has no Entitlements".into())
    })?;
    if entitlements.as_dictionary().is_none() {
        return Err(CoreError::InvalidArgument(
            "provisioning profile Entitlements is not a dictionary".into(),
        ));
    }
    let developer_certificates = dictionary
        .get("DeveloperCertificates")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_data)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let provisioned_devices = dictionary
        .get("ProvisionedDevices")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_string)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(ProvisioningProfile {
        entitlements,
        developer_certificates,
        provisioned_devices,
        provisions_all_devices: dictionary
            .get("ProvisionsAllDevices")
            .and_then(Value::as_boolean)
            .unwrap_or(false),
        expiration_date: dictionary
            .get("ExpirationDate")
            .and_then(Value::as_date)
            .map(Into::into),
    })
}

fn validate_profile(
    profile: &ProvisioningProfile,
    certificate_der: &[u8],
    device_udid: Option<&str>,
) -> CoreResult<()> {
    if profile
        .expiration_date
        .is_some_and(|expiration| expiration <= SystemTime::now())
    {
        return Err(CoreError::InvalidArgument(
            "provisioning profile has expired".into(),
        ));
    }
    if profile.developer_certificates.is_empty() {
        return Err(CoreError::InvalidArgument(
            "provisioning profile has no DeveloperCertificates".into(),
        ));
    }
    if !profile
        .developer_certificates
        .iter()
        .any(|candidate| candidate == certificate_der)
    {
        return Err(CoreError::InvalidArgument(
            "P12 certificate does not match provisioning profile".into(),
        ));
    }
    if let Some(udid) = device_udid
        && !profile.provisions_all_devices
        && !profile
            .provisioned_devices
            .iter()
            .any(|value| value == udid)
    {
        return Err(CoreError::PermissionDenied(format!(
            "device {udid} is not included in the provisioning profile"
        )));
    }
    Ok(())
}

fn validate_profile_bundle_id(profile: &ProvisioningProfile, bundle_id: &str) -> CoreResult<()> {
    let entitlements = profile.entitlements.as_dictionary().expect("validated");
    let application_identifier = entitlements
        .get("application-identifier")
        .and_then(Value::as_string)
        .or_else(|| {
            entitlements
                .get("com.apple.application-identifier")
                .and_then(Value::as_string)
        })
        .ok_or_else(|| {
            CoreError::InvalidArgument(
                "provisioning profile has no application-identifier entitlement".into(),
            )
        })?;
    let profile_bundle = application_identifier
        .split_once('.')
        .map(|(_, value)| value)
        .unwrap_or(application_identifier);
    let matches = profile_bundle == "*"
        || profile_bundle == bundle_id
        || profile_bundle
            .strip_suffix('*')
            .is_some_and(|prefix| bundle_id.starts_with(prefix));
    if !matches {
        return Err(CoreError::InvalidArgument(format!(
            "bundle ID {bundle_id:?} is not allowed by provisioning profile application-identifier {application_identifier:?}"
        )));
    }
    Ok(())
}

fn prepare_app_bundle(
    app_path: &Path,
    profile_data: &[u8],
    _profile: &ProvisioningProfile,
    requested_bundle_id: Option<&str>,
) -> CoreResult<String> {
    let info_path = app_path.join("Info.plist");
    let mut info = Value::from_file(&info_path).map_err(|error| {
        CoreError::InvalidArgument(format!("read {}: {error}", info_path.display()))
    })?;
    let dictionary = info.as_dictionary_mut().ok_or_else(|| {
        CoreError::InvalidArgument(format!("{} is not a plist dictionary", info_path.display()))
    })?;
    let bundle_id = requested_bundle_id
        .map(str::to_owned)
        .or_else(|| {
            dictionary
                .get("CFBundleIdentifier")
                .and_then(Value::as_string)
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            CoreError::InvalidArgument(format!("{} has no CFBundleIdentifier", info_path.display()))
        })?;
    dictionary.insert(
        "CFBundleIdentifier".into(),
        Value::String(bundle_id.clone()),
    );
    let mut info_file = File::create(&info_path)?;
    info.to_writer_xml(&mut info_file)
        .map_err(|error| CoreError::Backend(format!("write {}: {error}", info_path.display())))?;
    fs::write(app_path.join("embedded.mobileprovision"), profile_data)?;
    let old_signature = app_path.join("_CodeSignature");
    if old_signature.exists() {
        fs::remove_dir_all(old_signature)?;
    }
    Ok(bundle_id)
}

fn parse_sign_app_options(arguments: &[String]) -> CoreResult<ParsedCommandOptions> {
    let mut values = BTreeMap::new();
    let mut install = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "--install" {
            if install {
                return Err(CoreError::InvalidArgument(
                    "sign app --install was provided more than once".into(),
                ));
            }
            install = true;
            index += 1;
            continue;
        }
        if !argument.starts_with("--") {
            return Err(CoreError::InvalidArgument(format!(
                "sign app does not accept positional value {argument:?}"
            )));
        }
        let (name, value, consumed) = option_parts(arguments, index)?;
        if !SIGN_APP_VALUE_OPTIONS.contains(&name) {
            return Err(CoreError::InvalidArgument(format!(
                "unknown sign app option {name:?}"
            )));
        }
        if values.insert(name.into(), value).is_some() {
            return Err(CoreError::InvalidArgument(format!(
                "sign app option {name:?} was provided more than once"
            )));
        }
        index += consumed;
    }
    Ok(ParsedCommandOptions { values })
}

fn option_parts(arguments: &[String], index: usize) -> CoreResult<(&str, String, usize)> {
    let argument = &arguments[index];
    let (name, value, consumed) = if let Some((name, value)) = argument.split_once('=') {
        (name, value.to_owned(), 1)
    } else {
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| CoreError::InvalidArgument(format!("{argument} requires a value")))?;
        if value.starts_with("--") {
            return Err(CoreError::InvalidArgument(format!(
                "{argument} requires a value"
            )));
        }
        (argument.as_str(), value.clone(), 2)
    };
    if value.is_empty() {
        return Err(CoreError::InvalidArgument(format!(
            "{name} requires a non-empty value"
        )));
    }
    Ok((name, value, consumed))
}

fn required_path(
    invocation: &CommandInvocation,
    values: &BTreeMap<String, String>,
    name: &str,
    operation: &str,
) -> CoreResult<PathBuf> {
    values
        .get(name)
        .map(|value| resolve_client_path(invocation, value))
        .ok_or_else(|| CoreError::InvalidArgument(format!("{operation} requires {name}")))
}

pub(crate) fn resolve_client_path(invocation: &CommandInvocation, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if let Some(cwd) = &invocation.cwd {
        PathBuf::from(cwd).join(path)
    } else {
        path
    }
}

pub(crate) fn default_signed_output(input: &Path) -> PathBuf {
    let extension = input.extension().and_then(|value| value.to_str());
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("app");
    let name = match extension {
        Some(extension) => format!("{stem}-signed.{extension}"),
        None => format!("{stem}-signed"),
    };
    input.with_file_name(name)
}

pub(crate) fn extract_zip(
    archive_path: &Path,
    destination: &Path,
) -> CoreResult<HashMap<PathBuf, u32>> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file).map_err(zip_error)?;
    let mut modes = HashMap::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        let relative = entry.enclosed_name().ok_or_else(|| {
            CoreError::InvalidArgument(format!("zip entry escapes destination: {}", entry.name()))
        })?;
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(CoreError::InvalidArgument(format!(
                "zip entry has an unsafe path: {}",
                entry.name()
            )));
        }
        let target = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = File::create(&target)?;
            std::io::copy(&mut entry, &mut output)?;
        }
        if let Some(mode) = entry.unix_mode() {
            modes.insert(relative.to_owned(), mode);
        }
    }
    Ok(modes)
}

pub(crate) fn find_app_bundle(root: &Path) -> CoreResult<PathBuf> {
    if !root.exists() {
        return Err(CoreError::InvalidArgument(format!(
            "app search root does not exist: {}",
            root.display()
        )));
    }
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
                {
                    return Ok(path);
                }
                pending.push(path);
            }
        }
    }
    Err(CoreError::InvalidArgument(format!(
        "no .app bundle found under {}",
        root.display()
    )))
}

fn repackage_zip(root: &Path, output_path: &Path, modes: &HashMap<PathBuf, u32>) -> CoreResult<()> {
    let output = File::create(output_path)?;
    let mut writer = ZipWriter::new(output);
    let mut paths = Vec::new();
    collect_paths(root, root, &mut paths)?;
    paths.sort();
    for path in paths {
        let relative = path.strip_prefix(root).map_err(|error| {
            CoreError::Backend(format!("build IPA path for {}: {error}", path.display()))
        })?;
        let mut name = relative.to_string_lossy().replace('\\', "/");
        let metadata = fs::metadata(&path)?;
        let mode = modes
            .get(relative)
            .copied()
            .unwrap_or(if metadata.is_dir() { 0o755 } else { 0o644 });
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(mode);
        if metadata.is_dir() {
            name.push('/');
            writer.add_directory(name, options).map_err(zip_error)?;
        } else {
            writer.start_file(name, options).map_err(zip_error)?;
            let mut input = File::open(&path)?;
            std::io::copy(&mut input, &mut writer)?;
        }
    }
    writer.finish().map_err(zip_error)?;
    Ok(())
}

fn collect_paths(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> CoreResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|error| {
            CoreError::Backend(format!("walk archive path {}: {error}", path.display()))
        })?;
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(CoreError::Backend(format!(
                "archive source has unsafe path: {}",
                path.display()
            )));
        }
        output.push(path.clone());
        if entry.file_type()?.is_dir() {
            collect_paths(root, &path, output)?;
        }
    }
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> CoreResult<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else {
            return Err(CoreError::UnsupportedPlatform(format!(
                "app bundle contains an unsupported filesystem entry: {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn ensure_existing_path(path: &Path, label: &str) -> CoreResult<()> {
    if !path.exists() {
        return Err(CoreError::InvalidArgument(format!(
            "{label} path does not exist: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_existing_file(path: &Path, label: &str) -> CoreResult<()> {
    if !path.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "{label} path is not a file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> CoreResult<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn zip_error(error: zip::result::ZipError) -> CoreError {
    CoreError::InvalidArgument(format!("zip archive error: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    #[cfg(feature = "real-device")]
    use std::time::Duration;

    #[cfg(feature = "real-device")]
    use apple_codesign::macho_builder::MachOBuilder;
    #[cfg(feature = "real-device")]
    use cryptographic_message_syntax::{SignedDataBuilder, SignerBuilder};
    use plist::Dictionary;

    use super::*;

    fn invocation(arguments: &[&str], cwd: &Path) -> CommandInvocation {
        CommandInvocation {
            argv: arguments.iter().map(|value| (*value).into()).collect(),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            udid: None,
            output: Default::default(),
        }
    }

    #[test]
    fn sign_options_resolve_client_paths_and_reject_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let parsed = parse_sign_app_options(&[
            "--path=Runner.app".into(),
            "--p12file".into(),
            "identity.p12".into(),
            "--profile=runner.mobileprovision".into(),
        ])
        .unwrap();
        assert_eq!(parsed.values["--path"], "Runner.app");
        assert_eq!(parsed.values["--p12file"], "identity.p12");
        assert_eq!(
            required_path(
                &invocation(&["sign", "app"], directory.path()),
                &parsed.values,
                "--path",
                "sign app"
            )
            .unwrap(),
            directory.path().join("Runner.app")
        );
        assert!(parse_sign_app_options(&["--path=a.app".into(), "--path=b.app".into(),]).is_err());
    }

    #[test]
    fn zip_extraction_rejects_parent_paths_and_finds_app() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("runner.zip");
        {
            let output = File::create(&archive_path).unwrap();
            let mut writer = ZipWriter::new(output);
            writer
                .start_file(
                    "Payload/Runner.app/Info.plist",
                    SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"plist").unwrap();
            writer.finish().unwrap();
        }
        let extracted = directory.path().join("extracted");
        extract_zip(&archive_path, &extracted).unwrap();
        assert_eq!(
            find_app_bundle(&extracted).unwrap(),
            extracted.join("Payload/Runner.app")
        );
    }

    #[test]
    fn profile_bundle_validation_supports_exact_and_wildcard_ids() {
        let profile = |identifier: &str| ProvisioningProfile {
            entitlements: Value::Dictionary(Dictionary::from_iter([(
                "application-identifier".to_owned(),
                Value::String(identifier.into()),
            )])),
            developer_certificates: vec![vec![1]],
            provisioned_devices: Vec::new(),
            provisions_all_devices: true,
            expiration_date: None,
        };
        assert!(
            validate_profile_bundle_id(&profile("TEAM.com.example.Runner"), "com.example.Runner")
                .is_ok()
        );
        assert!(
            validate_profile_bundle_id(&profile("TEAM.com.example.*"), "com.example.Runner")
                .is_ok()
        );
        assert!(
            validate_profile_bundle_id(&profile("TEAM.com.other.App"), "com.example.Runner")
                .is_err()
        );
    }

    #[test]
    #[cfg(feature = "real-device")]
    fn signs_generated_app_with_cms_profile() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("Runner.app");
        fs::create_dir_all(&input).unwrap();
        let info = Value::Dictionary(Dictionary::from_iter([
            (
                "CFBundleIdentifier".to_owned(),
                Value::String("com.example.Original".into()),
            ),
            (
                "CFBundleExecutable".to_owned(),
                Value::String("Runner".into()),
            ),
            (
                "CFBundlePackageType".to_owned(),
                Value::String("APPL".into()),
            ),
        ]));
        info.to_file_xml(input.join("Info.plist")).unwrap();
        fs::write(
            input.join("Runner"),
            MachOBuilder::new_aarch64(2).write_macho().unwrap(),
        )
        .unwrap();

        let identity = crate::supervision::generate_identity("test-password").unwrap();
        let (certificate, private_key) =
            load_p12_identity(&identity.pkcs12_der, "test-password").unwrap();
        let profile_value = Value::Dictionary(Dictionary::from_iter([
            (
                "Entitlements".to_owned(),
                Value::Dictionary(Dictionary::from_iter([
                    (
                        "application-identifier".to_owned(),
                        Value::String("TESTTEAM.com.example.Runner".into()),
                    ),
                    ("get-task-allow".to_owned(), Value::Boolean(true)),
                ])),
            ),
            (
                "DeveloperCertificates".to_owned(),
                Value::Array(vec![Value::Data(certificate.constructed_data().to_vec())]),
            ),
            (
                "ProvisionedDevices".to_owned(),
                Value::Array(vec![Value::String("test-device".into())]),
            ),
            (
                "ExpirationDate".to_owned(),
                Value::Date((SystemTime::now() + Duration::from_secs(3600)).into()),
            ),
        ]));
        let mut profile_xml = Vec::new();
        profile_value.to_writer_xml(&mut profile_xml).unwrap();
        let profile_data = SignedDataBuilder::default()
            .content_inline(profile_xml)
            .certificate(certificate.clone())
            .signer(SignerBuilder::new(&private_key, certificate))
            .build_der()
            .unwrap();
        let p12_path = directory.path().join("identity.p12");
        let profile_path = directory.path().join("runner.mobileprovision");
        fs::write(&p12_path, identity.pkcs12_der).unwrap();
        fs::write(&profile_path, &profile_data).unwrap();
        let output = directory.path().join("Runner-signed.app");

        let result = sign_with_files_blocking(
            &input,
            &output,
            &p12_path,
            "test-password",
            &profile_path,
            Some("com.example.Runner"),
            Some("test-device"),
        )
        .unwrap();
        assert_eq!(result.bundle_id, "com.example.Runner");
        let signed_info = Value::from_file(output.join("Info.plist")).unwrap();
        assert_eq!(
            signed_info
                .as_dictionary()
                .unwrap()
                .get("CFBundleIdentifier")
                .and_then(Value::as_string),
            Some("com.example.Runner")
        );
        assert_eq!(
            fs::read(output.join("embedded.mobileprovision")).unwrap(),
            profile_data
        );
        assert!(output.join("_CodeSignature/CodeResources").is_file());
        assert!(fs::metadata(output.join("Runner")).unwrap().len() > 16_384);

        let ipa_root = directory.path().join("ipa-root");
        let ipa_app = ipa_root.join("Payload/Runner.app");
        copy_directory(&input, &ipa_app).unwrap();
        let input_ipa = directory.path().join("Runner.ipa");
        let mut modes = HashMap::new();
        modes.insert(PathBuf::from("Payload/Runner.app/Runner"), 0o755);
        repackage_zip(&ipa_root, &input_ipa, &modes).unwrap();
        let output_ipa = directory.path().join("Runner-signed.ipa");
        sign_with_files_blocking(
            &input_ipa,
            &output_ipa,
            &p12_path,
            "test-password",
            &profile_path,
            Some("com.example.Runner"),
            Some("test-device"),
        )
        .unwrap();
        let signed_ipa = directory.path().join("signed-ipa");
        let signed_modes = extract_zip(&output_ipa, &signed_ipa).unwrap();
        assert!(
            signed_ipa
                .join("Payload/Runner.app/_CodeSignature/CodeResources")
                .is_file()
        );
        assert_eq!(
            signed_modes
                .get(Path::new("Payload/Runner.app/Runner"))
                .map(|mode| mode & 0o777),
            Some(0o755)
        );
    }
}
