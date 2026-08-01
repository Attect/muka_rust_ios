use std::cmp::Ordering;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use plist::{Dictionary, Value};
use reqwest::{Client, Url};
use tokio::io::AsyncWriteExt;
use tracing::debug;

use crate::{CoreError, CoreResult};

const BUILD_MANIFEST: &str = "BuildManifest.plist";
const LEGACY_IMAGE: &str = "DeveloperDiskImage.dmg";
const LEGACY_SIGNATURE: &str = "DeveloperDiskImage.dmg.signature";
const LEGACY_REPOSITORY: &str = "https://raw.githubusercontent.com/mspvirajpatel/Xcode_Developer_Disk_Images/master/Developer%20Disk%20Image/";
const MAX_LEGACY_IMAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_LEGACY_SIGNATURE_BYTES: u64 = 64 * 1024;
const LEGACY_DDI_VERSIONS: &[&str] = &[
    "4.2",
    "4.3",
    "5.0",
    "5.1",
    "6.0",
    "6.1",
    "7.0",
    "7.1",
    "8.0",
    "8.1",
    "8.2",
    "8.3",
    "8.4 (12H141)",
    "9.0 (13A340)",
    "9.1 (13B5110e)",
    "9.2 (13C75)",
    "9.3 (13E230)",
    "10.0 (14A345)",
    "10.1 (14B72)",
    "10.2 (14C5062c)",
    "10.3 (14E269)",
    "11.0 (15A372)",
    "11.1 (15B87)",
    "11.2 (15C5092b)",
    "11.3 (15E5178d)",
    "11.4 (15F5037c)",
    "12.0 (16A5288q)",
    "12.1 (16B5059d)",
    "12.2 (16E5191d)",
    "12.3 (16F148)",
    "12.4",
    "13.0",
    "13.1",
    "13.2",
    "13.3",
    "13.4",
    "13.5",
    "13.7",
    "14.0",
    "14.1",
    "14.2",
    "14.4",
    "14.5",
    "14.6",
    "14.7",
    "14.7.1",
    "14.8",
    "15.0",
    "15.1",
    "15.2",
    "15.3",
    "15.3.1",
    "15.4",
    "15.5",
    "15.6",
    "15.6.1",
    "15.7",
    "16.0",
    "16.1",
    "16.2",
    "16.3",
    "16.4",
    "16.4.1",
    "16.5",
    "16.6",
];

#[derive(Debug, Clone)]
pub(crate) struct PersonalizedDdi {
    pub restore_dir: PathBuf,
    pub build_version: String,
    pub build_manifest: Vec<u8>,
    pub image_path: PathBuf,
    pub trust_cache_path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct LegacyDdi {
    pub version: String,
    pub image_path: PathBuf,
    pub signature_path: PathBuf,
    pub downloaded: bool,
}

pub(crate) async fn download_legacy_ddi(
    base_dir: &Path,
    product_version: &str,
) -> CoreResult<LegacyDdi> {
    let repository = Url::parse(LEGACY_REPOSITORY)
        .map_err(|error| CoreError::Backend(format!("invalid legacy DDI repository: {error}")))?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| CoreError::Backend(format!("build legacy DDI HTTP client: {error}")))?;
    download_legacy_ddi_from_repository(&client, &repository, base_dir, product_version).await
}

async fn download_legacy_ddi_from_repository(
    client: &Client,
    repository: &Url,
    base_dir: &Path,
    product_version: &str,
) -> CoreResult<LegacyDdi> {
    if !matches!(repository.scheme(), "http" | "https") {
        return Err(CoreError::InvalidArgument(
            "legacy DDI repository must use http or https".into(),
        ));
    }
    let version = match_legacy_ddi_version(product_version)?;
    let short_version = version.split_once(" (").map_or(version, |(value, _)| value);
    for directory in [version, short_version] {
        let candidate = base_dir.join(directory);
        if let Some(ddi) = cached_legacy_ddi(&candidate, version)? {
            return Ok(ddi);
        }
    }

    let target_dir = base_dir.join(short_version);
    ensure_safe_cache_directory(&target_dir)?;
    tokio::fs::create_dir_all(&target_dir).await?;
    let image_path = target_dir.join(LEGACY_IMAGE);
    let signature_path = target_dir.join(LEGACY_SIGNATURE);
    let image_url = legacy_repository_url(repository, version, LEGACY_IMAGE)?;
    let signature_url = legacy_repository_url(repository, version, LEGACY_SIGNATURE)?;
    let image_temp = download_file_bounded(
        client,
        &image_url,
        &target_dir,
        LEGACY_IMAGE,
        MAX_LEGACY_IMAGE_BYTES,
    )
    .await?;
    if let Err(error) = validate_legacy_image(&image_temp) {
        let _ = tokio::fs::remove_file(&image_temp).await;
        return Err(error);
    }
    let signature_temp = match download_file_bounded(
        client,
        &signature_url,
        &target_dir,
        LEGACY_SIGNATURE,
        MAX_LEGACY_SIGNATURE_BYTES,
    )
    .await
    {
        Ok(path) => path,
        Err(error) => {
            let _ = tokio::fs::remove_file(&image_temp).await;
            return Err(error);
        }
    };
    if let Err(error) = validate_legacy_signature(&signature_temp) {
        let _ = tokio::fs::remove_file(&image_temp).await;
        let _ = tokio::fs::remove_file(&signature_temp).await;
        return Err(error);
    }

    let mut promoted_signature = false;
    let mut promoted_image = false;
    let promotion = async {
        replace_cache_file(&signature_temp, &signature_path).await?;
        promoted_signature = true;
        replace_cache_file(&image_temp, &image_path).await?;
        promoted_image = true;
        validate_legacy_image(&image_path)?;
        validate_legacy_signature(&signature_path)?;
        Ok::<(), CoreError>(())
    }
    .await;
    if promotion.is_err() {
        let _ = tokio::fs::remove_file(&image_temp).await;
        let _ = tokio::fs::remove_file(&signature_temp).await;
        if promoted_image {
            let _ = tokio::fs::remove_file(&image_path).await;
        }
        if promoted_signature {
            let _ = tokio::fs::remove_file(&signature_path).await;
        }
    }
    promotion?;
    Ok(LegacyDdi {
        version: version.into(),
        image_path,
        signature_path,
        downloaded: true,
    })
}

fn match_legacy_ddi_version(product_version: &str) -> CoreResult<&'static str> {
    let requested = parse_ios_version(product_version).ok_or_else(|| {
        CoreError::Backend(format!(
            "Lockdown ProductVersion {product_version:?} has an invalid format"
        ))
    })?;
    if requested.first().copied().unwrap_or_default() >= 17 {
        return Err(CoreError::InvalidArgument(format!(
            "iOS {product_version} requires a personalized Developer Disk Image"
        )));
    }
    // 实机验证表明 iOS 15.8 使用 Xcode 15.5 legacy DDI 可挂载，15.7 cache 可能被拒绝。
    if requested.first() == Some(&15) && requested.get(1) == Some(&8) {
        return Ok("15.5");
    }
    LEGACY_DDI_VERSIONS
        .iter()
        .copied()
        .filter(|candidate| {
            parse_ios_version(candidate)
                .is_some_and(|candidate| compare_ios_versions(&candidate, &requested) != Ordering::Greater)
        })
        .max_by(|left, right| {
            compare_ios_versions(
                &parse_ios_version(left).expect("known legacy DDI version"),
                &parse_ios_version(right).expect("known legacy DDI version"),
            )
        })
        .ok_or_else(|| {
            CoreError::DeviceUnavailable(format!(
                "no legacy Developer Disk Image is available for iOS {product_version}; oldest supported image is 4.2"
            ))
        })
}

fn parse_ios_version(value: &str) -> Option<Vec<u64>> {
    let numeric = value.split_once(" (").map_or(value, |(value, _)| value);
    let parts: Vec<_> = numeric
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<_, _>>()
        .ok()?;
    (!parts.is_empty()).then_some(parts)
}

fn compare_ios_versions(left: &[u64], right: &[u64]) -> Ordering {
    let length = left.len().max(right.len());
    (0..length)
        .map(|index| {
            left.get(index)
                .copied()
                .unwrap_or_default()
                .cmp(&right.get(index).copied().unwrap_or_default())
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn cached_legacy_ddi(directory: &Path, version: &str) -> CoreResult<Option<LegacyDdi>> {
    ensure_safe_cache_directory(directory)?;
    let image_path = directory.join(LEGACY_IMAGE);
    let signature_path = directory.join(LEGACY_SIGNATURE);
    if !image_path.exists() || !signature_path.exists() {
        return Ok(None);
    }
    if validate_legacy_image(&image_path).is_err()
        || validate_legacy_signature(&signature_path).is_err()
    {
        return Ok(None);
    }
    Ok(Some(LegacyDdi {
        version: version.into(),
        image_path,
        signature_path,
        downloaded: false,
    }))
}

fn ensure_safe_cache_directory(directory: &Path) -> CoreResult<()> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(CoreError::InvalidArgument(format!(
                "refusing to use legacy DDI cache symlink {}",
                directory.display()
            )))
        }
        Ok(metadata) if !metadata.is_dir() => Err(CoreError::InvalidArgument(format!(
            "legacy DDI cache path {} is not a directory",
            directory.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn legacy_repository_url(repository: &Url, version: &str, file: &str) -> CoreResult<Url> {
    let mut url = repository.clone();
    let mut segments = url.path_segments_mut().map_err(|_| {
        CoreError::InvalidArgument("legacy DDI repository cannot be a base URL".into())
    })?;
    segments.pop_if_empty().push(version).push(file);
    drop(segments);
    Ok(url)
}

async fn download_file_bounded(
    client: &Client,
    url: &Url,
    directory: &Path,
    file_name: &str,
    maximum: u64,
) -> CoreResult<PathBuf> {
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|error| CoreError::DeviceUnavailable(format!("download {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(CoreError::DeviceUnavailable(format!(
            "download {url} returned HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum)
    {
        return Err(CoreError::DeviceUnavailable(format!(
            "download {url} exceeds {} bytes",
            maximum
        )));
    }
    let temporary = directory.join(format!(".{file_name}.part-{}", uuid::Uuid::new_v4()));
    let result = async {
        let mut output = tokio::fs::File::create(&temporary).await?;
        let mut written = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            CoreError::DeviceUnavailable(format!("read download {url}: {error}"))
        })? {
            written = written.saturating_add(chunk.len() as u64);
            if written > maximum {
                return Err(CoreError::DeviceUnavailable(format!(
                    "download {url} exceeds {maximum} bytes"
                )));
            }
            output.write_all(&chunk).await?;
        }
        output.flush().await?;
        drop(output);
        if written == 0 {
            return Err(CoreError::DeviceUnavailable(format!(
                "download {url} returned an empty body"
            )));
        }
        Ok::<(), CoreError>(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result?;
    Ok(temporary)
}

async fn replace_cache_file(source: &Path, destination: &Path) -> CoreResult<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(CoreError::InvalidArgument(format!(
                "legacy DDI cache file {} must be a regular file",
                destination.display()
            )));
        }
        tokio::fs::remove_file(destination).await?;
    }
    tokio::fs::rename(source, destination).await?;
    Ok(())
}

fn validate_legacy_image(path: &Path) -> CoreResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "legacy Developer Disk Image {} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() < 512 || metadata.len() > MAX_LEGACY_IMAGE_BYTES {
        return Err(CoreError::Backend(format!(
            "legacy Developer Disk Image {} has invalid size {}",
            path.display(),
            metadata.len()
        )));
    }
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::End(-512))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    if &magic != b"koly" {
        return Err(CoreError::Backend(format!(
            "legacy Developer Disk Image {} has no UDIF koly trailer",
            path.display()
        )));
    }
    Ok(())
}

fn validate_legacy_signature(path: &Path) -> CoreResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "legacy Developer Disk Image signature {} must be a regular file",
            path.display()
        )));
    }
    if !(16..=MAX_LEGACY_SIGNATURE_BYTES).contains(&metadata.len()) {
        return Err(CoreError::Backend(format!(
            "legacy Developer Disk Image signature {} has invalid size {}",
            path.display(),
            metadata.len()
        )));
    }
    Ok(())
}

pub(crate) fn load_personalized_ddi(
    path: &Path,
    board_id: u64,
    chip_id: u64,
) -> CoreResult<PersonalizedDdi> {
    let restore_dir = resolve_restore_dir(path)?;
    inspect_restore_dir(&restore_dir, board_id, chip_id)?.ok_or_else(|| {
        CoreError::InvalidArgument(format!(
            "{} has no BuildIdentity for BoardId 0x{board_id:x} and ChipID 0x{chip_id:x}",
            restore_dir.display()
        ))
    })
}

pub(crate) fn find_best_personalized_ddi(
    base_dir: &Path,
    board_id: u64,
    chip_id: u64,
) -> CoreResult<Option<PersonalizedDdi>> {
    let entries = match fs::read_dir(base_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let mut best: Option<PersonalizedDdi> = None;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.file_name().to_string_lossy().starts_with("ddi-")
        {
            continue;
        }
        let restore_dir = entry.path().join("Restore");
        let candidate = match inspect_restore_dir(&restore_dir, board_id, chip_id) {
            Ok(Some(candidate)) => candidate,
            Ok(None) => continue,
            Err(error) => {
                debug!(path = %restore_dir.display(), %error, "ignoring unusable local DDI");
                continue;
            }
        };
        let replace = best.as_ref().is_none_or(|current| {
            match compare_build_versions(&candidate.build_version, &current.build_version) {
                Ordering::Greater => true,
                Ordering::Equal => candidate.restore_dir > current.restore_dir,
                Ordering::Less => false,
            }
        });
        if replace {
            best = Some(candidate);
        }
    }
    Ok(best)
}

fn resolve_restore_dir(path: &Path) -> CoreResult<PathBuf> {
    if path.join(BUILD_MANIFEST).is_file() {
        return Ok(path.to_owned());
    }
    let nested = path.join("Restore");
    if nested.join(BUILD_MANIFEST).is_file() {
        return Ok(nested);
    }
    Err(CoreError::InvalidArgument(format!(
        "personalized image path {} must be a Restore directory containing {BUILD_MANIFEST}",
        path.display()
    )))
}

fn inspect_restore_dir(
    restore_dir: &Path,
    board_id: u64,
    chip_id: u64,
) -> CoreResult<Option<PersonalizedDdi>> {
    let manifest_path = restore_dir.join(BUILD_MANIFEST);
    let build_manifest = fs::read(&manifest_path).map_err(|error| {
        CoreError::Io(std::io::Error::new(
            error.kind(),
            format!("cannot read {}: {error}", manifest_path.display()),
        ))
    })?;
    let manifest: Value = plist::from_bytes(&build_manifest).map_err(|error| {
        CoreError::Backend(format!("cannot parse {}: {error}", manifest_path.display()))
    })?;
    let manifest = manifest.as_dictionary().ok_or_else(|| {
        CoreError::Backend(format!(
            "{} does not contain a plist dictionary",
            manifest_path.display()
        ))
    })?;
    let Some(identity) = find_identity(manifest, board_id, chip_id) else {
        return Ok(None);
    };

    let image_relative = component_path(identity, &["PersonalizedDMG", "PersonalizedDmg"])?;
    let trust_cache_relative = component_path(identity, &["LoadableTrustCache"])?;
    let image_path = safe_manifest_path(restore_dir, &image_relative)?;
    let trust_cache_path = safe_manifest_path(restore_dir, &trust_cache_relative)?;
    require_file(&image_path, "PersonalizedDMG")?;
    require_file(&trust_cache_path, "LoadableTrustCache")?;

    let build_version = manifest
        .get("ProductBuildVersion")
        .and_then(Value::as_string)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            restore_dir
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("ddi-"))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            CoreError::Backend(format!(
                "{} has no ProductBuildVersion",
                manifest_path.display()
            ))
        })?;

    Ok(Some(PersonalizedDdi {
        restore_dir: restore_dir.to_owned(),
        build_version,
        build_manifest,
        image_path,
        trust_cache_path,
    }))
}

fn find_identity(manifest: &Dictionary, board_id: u64, chip_id: u64) -> Option<&Dictionary> {
    manifest
        .get("BuildIdentities")?
        .as_array()?
        .iter()
        .filter_map(Value::as_dictionary)
        .find(|identity| {
            parse_manifest_id(identity.get("ApBoardID")) == Some(board_id)
                && parse_manifest_id(identity.get("ApChipID")) == Some(chip_id)
        })
}

fn parse_manifest_id(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::String(value)) => u64::from_str_radix(
            value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
                .unwrap_or(value),
            16,
        )
        .ok(),
        Some(Value::Integer(value)) => value.as_unsigned().or_else(|| {
            value
                .as_signed()
                .and_then(|value| u64::try_from(value).ok())
        }),
        _ => None,
    }
}

fn component_path(identity: &Dictionary, names: &[&str]) -> CoreResult<String> {
    let manifest = identity
        .get("Manifest")
        .and_then(Value::as_dictionary)
        .ok_or_else(|| CoreError::Backend("BuildIdentity has no Manifest dictionary".into()))?;
    for name in names {
        if let Some(path) = manifest
            .get(name)
            .and_then(Value::as_dictionary)
            .and_then(|entry| entry.get("Info"))
            .and_then(Value::as_dictionary)
            .and_then(|info| info.get("Path"))
            .and_then(Value::as_string)
            .filter(|path| !path.is_empty())
        {
            return Ok(path.to_owned());
        }
    }
    Err(CoreError::Backend(format!(
        "BuildIdentity has no {} component path",
        names.join(" or ")
    )))
}

fn safe_manifest_path(restore_dir: &Path, relative: &str) -> CoreResult<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CoreError::Backend(format!(
            "BuildManifest contains unsafe component path {relative:?}"
        )));
    }
    Ok(restore_dir.join(relative))
}

fn require_file(path: &Path, component: &str) -> CoreResult<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(CoreError::InvalidArgument(format!(
            "{component} file {} does not exist",
            path.display()
        )))
    }
}

pub(crate) fn compare_build_versions(left: &str, right: &str) -> Ordering {
    let left = split_build_version(left);
    let right = split_build_version(right);
    for (left, right) in left.iter().zip(&right) {
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            _ => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

fn split_build_version(value: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let bytes = value.as_bytes();
    for index in 1..bytes.len() {
        if bytes[index].is_ascii_digit() != bytes[index - 1].is_ascii_digit() {
            result.push(&value[start..index]);
            start = index;
        }
    }
    if !value.is_empty() {
        result.push(&value[start..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use axum::Router;
    use axum::extract::{Path as AxumPath, State};
    use axum::http::StatusCode;
    use axum::routing::get;

    use super::*;

    fn dictionary<const N: usize>(entries: [(&str, Value); N]) -> Dictionary {
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    fn component(path: &str) -> Value {
        Value::Dictionary(dictionary([(
            "Info",
            Value::Dictionary(dictionary([("Path", Value::String(path.to_owned()))])),
        )]))
    }

    fn write_ddi(
        base: &Path,
        directory: &str,
        build: Option<&str>,
        board_id: &str,
        chip_id: &str,
    ) -> PathBuf {
        let restore = base.join(directory).join("Restore");
        fs::create_dir_all(&restore).unwrap();
        fs::write(restore.join("DeveloperDiskImage.dmg"), b"image").unwrap();
        fs::write(restore.join("DeveloperDiskImage.dmg.trustcache"), b"trust").unwrap();

        let mut manifest = Dictionary::new();
        if let Some(build) = build {
            manifest.insert("ProductBuildVersion".into(), build.into());
        }
        manifest.insert(
            "BuildIdentities".into(),
            Value::Array(vec![Value::Dictionary(dictionary([
                ("ApBoardID", Value::String(board_id.to_owned())),
                ("ApChipID", Value::String(chip_id.to_owned())),
                (
                    "Manifest",
                    Value::Dictionary(dictionary([
                        ("PersonalizedDMG", component("DeveloperDiskImage.dmg")),
                        (
                            "LoadableTrustCache",
                            component("DeveloperDiskImage.dmg.trustcache"),
                        ),
                    ])),
                ),
            ]))]),
        );
        let mut file = fs::File::create(restore.join(BUILD_MANIFEST)).unwrap();
        Value::Dictionary(manifest)
            .to_writer_xml(&mut file)
            .unwrap();
        restore
    }

    fn legacy_image_bytes() -> Vec<u8> {
        let mut image = vec![0_u8; 1024];
        image[512..516].copy_from_slice(b"koly");
        image
    }

    async fn legacy_download(
        State(requests): State<Arc<AtomicUsize>>,
        AxumPath((_version, file)): AxumPath<(String, String)>,
    ) -> (StatusCode, Vec<u8>) {
        requests.fetch_add(1, AtomicOrdering::SeqCst);
        match file.as_str() {
            LEGACY_IMAGE => (StatusCode::OK, legacy_image_bytes()),
            LEGACY_SIGNATURE => (StatusCode::OK, vec![0x5a; 128]),
            _ => (StatusCode::NOT_FOUND, Vec::new()),
        }
    }

    async fn invalid_legacy_download(
        AxumPath((_version, file)): AxumPath<(String, String)>,
    ) -> (StatusCode, Vec<u8>) {
        match file.as_str() {
            LEGACY_IMAGE => (StatusCode::OK, vec![0_u8; 1024]),
            LEGACY_SIGNATURE => (StatusCode::OK, vec![0x5a; 128]),
            _ => (StatusCode::NOT_FOUND, Vec::new()),
        }
    }

    #[test]
    fn apple_build_versions_compare_by_numeric_and_alpha_segments() {
        assert_eq!(
            compare_build_versions("17F113", "17E5179g"),
            Ordering::Greater
        );
        assert_eq!(compare_build_versions("17F113", "17F84"), Ordering::Greater);
        assert_eq!(compare_build_versions("23F84", "17F113"), Ordering::Greater);
        assert_eq!(
            compare_build_versions("17E5179g", "17E5179f"),
            Ordering::Greater
        );
        assert_eq!(compare_build_versions("17F113", "17F113"), Ordering::Equal);
        assert_eq!(compare_build_versions("17F84", "17F113"), Ordering::Less);
    }

    #[test]
    fn legacy_image_matching_follows_go_ios_compatible_floor_versions() {
        assert_eq!(
            match_legacy_ddi_version("11.2.5").unwrap(),
            "11.2 (15C5092b)"
        );
        assert_eq!(match_legacy_ddi_version("13.6.1").unwrap(), "13.5");
        assert_eq!(match_legacy_ddi_version("15.4.1").unwrap(), "15.4");
        assert_eq!(match_legacy_ddi_version("15.8").unwrap(), "15.5");
        assert_eq!(match_legacy_ddi_version("15.8.1").unwrap(), "15.5");
        assert_eq!(match_legacy_ddi_version("16.7.2").unwrap(), "16.6");
        assert!(match_legacy_ddi_version("4.1").is_err());
        assert!(match_legacy_ddi_version("17.0").is_err());
        assert!(match_legacy_ddi_version("invalid").is_err());
    }

    #[tokio::test]
    async fn legacy_image_download_is_bounded_atomic_and_reuses_cache() {
        let requests = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/{version}/{file}", get(legacy_download))
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let repository = Url::parse(&format!("http://{address}/")).unwrap();
        let client = Client::builder().build().unwrap();
        let directory = tempfile::tempdir().unwrap();

        let downloaded =
            download_legacy_ddi_from_repository(&client, &repository, directory.path(), "11.2.5")
                .await
                .unwrap();
        assert!(downloaded.downloaded);
        assert_eq!(downloaded.version, "11.2 (15C5092b)");
        assert_eq!(
            downloaded.image_path,
            directory.path().join("11.2").join(LEGACY_IMAGE)
        );
        validate_legacy_image(&downloaded.image_path).unwrap();
        validate_legacy_signature(&downloaded.signature_path).unwrap();
        assert_eq!(requests.load(AtomicOrdering::SeqCst), 2);

        server.abort();
        let _ = server.await;
        let cached =
            download_legacy_ddi_from_repository(&client, &repository, directory.path(), "11.2.5")
                .await
                .unwrap();
        assert!(!cached.downloaded);
        assert_eq!(cached.image_path, downloaded.image_path);
        assert_eq!(requests.load(AtomicOrdering::SeqCst), 2);
        assert!(
            fs::read_dir(directory.path().join("11.2"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".part-"))
        );
    }

    #[tokio::test]
    async fn legacy_image_download_rejects_invalid_dmg_without_cache_artifacts() {
        let router = Router::new().route("/{version}/{file}", get(invalid_legacy_download));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let repository = Url::parse(&format!("http://{address}/")).unwrap();
        let client = Client::builder().build().unwrap();
        let directory = tempfile::tempdir().unwrap();

        let error =
            download_legacy_ddi_from_repository(&client, &repository, directory.path(), "16.6")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("koly"));
        let cache = directory.path().join("16.6");
        assert!(!cache.join(LEGACY_IMAGE).exists());
        assert!(!cache.join(LEGACY_SIGNATURE).exists());
        assert!(fs::read_dir(cache).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".part-")
        }));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn legacy_image_promotion_failure_rolls_back_promoted_signature() {
        let requests = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/{version}/{file}", get(legacy_download))
            .with_state(requests);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let repository = Url::parse(&format!("http://{address}/")).unwrap();
        let client = Client::builder().build().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("16.6");
        fs::create_dir_all(cache.join(LEGACY_IMAGE)).unwrap();

        let error =
            download_legacy_ddi_from_repository(&client, &repository, directory.path(), "16.6")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("must be a regular file"));
        assert!(cache.join(LEGACY_IMAGE).is_dir());
        assert!(!cache.join(LEGACY_SIGNATURE).exists());
        assert!(fs::read_dir(cache).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".part-")
        }));
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn selects_newest_compatible_local_ddi_for_ios_26() {
        let directory = tempfile::tempdir().unwrap();
        write_ddi(
            directory.path(),
            "ddi-17E5179g",
            Some("17E5179g"),
            "0x10",
            "0x20",
        );
        let expected = write_ddi(directory.path(), "ddi-23F84", Some("23F84"), "0x10", "0x20");
        write_ddi(directory.path(), "ddi-24A1", Some("24A1"), "0x99", "0x20");

        let selected = find_best_personalized_ddi(directory.path(), 0x10, 0x20)
            .unwrap()
            .unwrap();
        assert_eq!(selected.restore_dir, expected);
        assert_eq!(selected.build_version, "23F84");
    }

    #[test]
    fn directory_name_is_a_build_version_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let restore = write_ddi(directory.path(), "ddi-23F84", None, "0x10", "0x20");
        let selected = load_personalized_ddi(&restore, 0x10, 0x20).unwrap();
        assert_eq!(selected.build_version, "23F84");
    }

    #[test]
    fn explicit_mount_rejects_an_incompatible_identity() {
        let directory = tempfile::tempdir().unwrap();
        let restore = write_ddi(directory.path(), "ddi-23F84", Some("23F84"), "0x10", "0x20");
        let error = load_personalized_ddi(&restore, 0x11, 0x20).unwrap_err();
        assert!(error.to_string().contains("has no BuildIdentity"));
    }

    #[test]
    fn uppercase_hex_identifiers_are_supported() {
        let directory = tempfile::tempdir().unwrap();
        let restore = write_ddi(directory.path(), "ddi-23F84", Some("23F84"), "0X10", "0X20");
        assert!(load_personalized_ddi(&restore, 0x10, 0x20).is_ok());
    }
}
