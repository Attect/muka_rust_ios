use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rust_ios_protocol::CommandInvocation;
use uuid::Uuid;

use crate::{CoreError, CoreResult};

pub(crate) const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

pub(crate) const MOCK_APP_ICON_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppIconRequest {
    pub bundle_identifier: String,
    pub output_path: PathBuf,
}

pub(crate) fn parse_app_icon_request(invocation: &CommandInvocation) -> CoreResult<AppIconRequest> {
    let mut bundle_identifier = None;
    let mut output = None;
    let mut index = 1;
    while index < invocation.argv.len() {
        let argument = &invocation.argv[index];
        if argument == "--output" {
            if output.is_some() {
                return Err(CoreError::InvalidArgument(
                    "get-app-icon --output was provided more than once".into(),
                ));
            }
            index += 1;
            let value = invocation.argv.get(index).ok_or_else(|| {
                CoreError::InvalidArgument("get-app-icon --output requires a file path".into())
            })?;
            if value.starts_with('-') {
                return Err(CoreError::InvalidArgument(
                    "get-app-icon --output requires a file path".into(),
                ));
            }
            output = Some(value.clone());
        } else if let Some(value) = argument.strip_prefix("--output=") {
            if output.is_some() {
                return Err(CoreError::InvalidArgument(
                    "get-app-icon --output was provided more than once".into(),
                ));
            }
            output = Some(value.to_owned());
        } else if argument.starts_with('-') {
            return Err(CoreError::InvalidArgument(format!(
                "get-app-icon does not accept {argument:?}"
            )));
        } else if bundle_identifier.replace(argument.clone()).is_some() {
            return Err(CoreError::InvalidArgument(format!(
                "get-app-icon accepts exactly one bundle ID; unexpected argument {argument:?}"
            )));
        }
        index += 1;
    }

    let bundle_identifier = bundle_identifier.ok_or_else(|| {
        CoreError::InvalidArgument("get-app-icon requires a non-empty bundle ID".into())
    })?;
    if bundle_identifier.is_empty()
        || bundle_identifier.split('.').any(|component| {
            component.is_empty()
                || !component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(CoreError::InvalidArgument(format!(
            "get-app-icon bundle ID {bundle_identifier:?} is invalid"
        )));
    }

    let output = output.unwrap_or_else(|| format!("{bundle_identifier}.png"));
    if output.trim().is_empty() || output == "-" {
        return Err(CoreError::InvalidArgument(
            "get-app-icon --output must be a file path; binary stdout is not supported".into(),
        ));
    }
    let output_path = resolve_client_path(invocation, &output);
    if output_path.file_name().is_none() {
        return Err(CoreError::InvalidArgument(format!(
            "get-app-icon output {} is not a file path",
            output_path.display()
        )));
    }

    Ok(AppIconRequest {
        bundle_identifier,
        output_path,
    })
}

pub(crate) fn validate_png(bytes: &[u8], context: &str) -> CoreResult<()> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err(CoreError::Backend(format!(
            "{context} returned {} bytes without a PNG signature",
            bytes.len()
        )));
    }
    Ok(())
}

pub(crate) async fn write_png_atomic(path: &Path, bytes: &[u8], context: &str) -> CoreResult<()> {
    validate_png(bytes, context)?;
    let path = path.to_owned();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || write_atomic(&path, &bytes))
        .await
        .map_err(|error| CoreError::Backend(format!("write PNG task failed: {error}")))??;
    Ok(())
}

fn resolve_client_path(invocation: &CommandInvocation, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else if let Some(cwd) = &invocation.cwd {
        Path::new(cwd).join(path)
    } else {
        path.to_owned()
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> CoreResult<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("app-icon.png");
    let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temporary, path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_ios_protocol::OutputOptions;

    fn invocation(arguments: &[&str], cwd: Option<&Path>) -> CommandInvocation {
        CommandInvocation {
            argv: arguments.iter().map(|value| (*value).into()).collect(),
            cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
            udid: None,
            output: OutputOptions::default(),
        }
    }

    #[test]
    fn app_icon_request_resolves_default_and_explicit_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let default = parse_app_icon_request(&invocation(
            &["get-app-icon", "com.apple.Preferences"],
            Some(directory.path()),
        ))
        .unwrap();
        assert_eq!(default.bundle_identifier, "com.apple.Preferences");
        assert_eq!(
            default.output_path,
            directory.path().join("com.apple.Preferences.png")
        );

        let explicit = parse_app_icon_request(&invocation(
            &[
                "get-app-icon",
                "com.apple.Preferences",
                "--output",
                "icons/settings.png",
            ],
            Some(directory.path()),
        ))
        .unwrap();
        assert_eq!(
            explicit.output_path,
            directory.path().join("icons/settings.png")
        );
    }

    #[test]
    fn app_icon_request_rejects_ambiguous_or_binary_output_arguments() {
        for arguments in [
            vec!["get-app-icon"],
            vec!["get-app-icon", ""],
            vec!["get-app-icon", "com/apple/Preferences"],
            vec!["get-app-icon", "com.apple.Preferences", "extra"],
            vec!["get-app-icon", "com.apple.Preferences", "--unknown"],
            vec!["get-app-icon", "com.apple.Preferences", "--output=-"],
            vec![
                "get-app-icon",
                "com.apple.Preferences",
                "--output=one.png",
                "--output=two.png",
            ],
        ] {
            assert!(parse_app_icon_request(&invocation(&arguments, None)).is_err());
        }
    }

    #[tokio::test]
    async fn atomic_png_write_replaces_complete_files_and_rejects_invalid_data() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("nested/icon.png");
        write_png_atomic(&output, MOCK_APP_ICON_PNG, "mock icon")
            .await
            .unwrap();
        assert_eq!(fs::read(&output).unwrap(), MOCK_APP_ICON_PNG);

        let error = write_png_atomic(&output, b"not a PNG", "bad icon")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("without a PNG signature"));
        assert_eq!(fs::read(&output).unwrap(), MOCK_APP_ICON_PNG);
        assert!(
            fs::read_dir(output.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp-"))
        );
    }

    #[cfg(feature = "real-device")]
    #[test]
    fn mock_icon_fixture_is_a_decodable_png() {
        let decoded =
            image::load_from_memory_with_format(MOCK_APP_ICON_PNG, image::ImageFormat::Png)
                .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 1));
    }
}
