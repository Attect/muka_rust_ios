use std::path::{Path, PathBuf};

use rust_ios_protocol::{CommandInvocation, DebugAppInfo};

use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalDebugApp {
    pub bundle_id: String,
    pub app_path: PathBuf,
    pub executable_path: PathBuf,
    pub executable_name: String,
}

pub(crate) fn local_debug_app(invocation: &CommandInvocation) -> CoreResult<LocalDebugApp> {
    let mut app_path = None;
    let mut stop_at_entry = false;
    for argument in invocation.argv.iter().skip(1) {
        match argument.as_str() {
            "--stop-at-entry" if !stop_at_entry => stop_at_entry = true,
            "--stop-at-entry" => {
                return Err(CoreError::InvalidArgument(
                    "debug --stop-at-entry was provided more than once".into(),
                ));
            }
            value if value.starts_with('-') => {
                return Err(CoreError::InvalidArgument(format!(
                    "debug does not accept option {value:?}"
                )));
            }
            value => {
                if app_path.replace(value).is_some() {
                    return Err(CoreError::InvalidArgument(
                        "debug accepts exactly one app_path".into(),
                    ));
                }
            }
        }
    }
    let raw_path =
        app_path.ok_or_else(|| CoreError::InvalidArgument("debug requires app_path".into()))?;
    let app_path = resolve_client_path(invocation, raw_path);
    if !app_path.is_dir() {
        return Err(CoreError::InvalidArgument(format!(
            "debug app_path is not a directory: {}",
            app_path.display()
        )));
    }
    let app_path = std::fs::canonicalize(&app_path).map_err(|error| {
        CoreError::InvalidArgument(format!(
            "cannot resolve debug app_path {}: {error}",
            app_path.display()
        ))
    })?;
    let info_path = app_path.join("Info.plist");
    if !info_path.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "debug app bundle has no Info.plist: {}",
            info_path.display()
        )));
    }
    let info = plist::Value::from_file(&info_path).map_err(|error| {
        CoreError::InvalidArgument(format!("parse {}: {error}", info_path.display()))
    })?;
    let dictionary = info.as_dictionary().ok_or_else(|| {
        CoreError::InvalidArgument(format!("{} is not a plist dictionary", info_path.display()))
    })?;
    let bundle_id = required_plist_string(dictionary, "CFBundleIdentifier", &info_path)?;
    let executable_name = required_plist_string(dictionary, "CFBundleExecutable", &info_path)?;
    if executable_name.contains('/') || executable_name.contains('\\') {
        return Err(CoreError::InvalidArgument(format!(
            "CFBundleExecutable in {} must be a file name",
            info_path.display()
        )));
    }
    let executable_path = app_path.join(&executable_name);
    if !executable_path.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "debug app executable does not exist: {}",
            executable_path.display()
        )));
    }
    Ok(LocalDebugApp {
        bundle_id,
        app_path,
        executable_path,
        executable_name,
    })
}

pub(crate) fn mock_debug_app_info(invocation: &CommandInvocation) -> CoreResult<DebugAppInfo> {
    let local = local_debug_app(invocation)?;
    let app_name = local
        .app_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| CoreError::InvalidArgument("debug app_path has no UTF-8 name".into()))?;
    let device_app_path = format!("/private/var/containers/Bundle/Application/MOCK-APP/{app_name}");
    Ok(DebugAppInfo {
        bundle_id: local.bundle_id,
        local_app_path: path_string(&local.app_path),
        local_executable_path: path_string(&local.executable_path),
        device_executable_path: format!("{device_app_path}/{}", local.executable_name),
        device_app_path,
    })
}

pub(crate) fn path_string(path: &Path) -> String {
    let value = path.to_string_lossy();
    if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    }
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

fn required_plist_string(
    dictionary: &plist::Dictionary,
    key: &str,
    info_path: &Path,
) -> CoreResult<String> {
    dictionary
        .get(key)
        .and_then(plist::Value::as_string)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            CoreError::InvalidArgument(format!("{} has no non-empty {key}", info_path.display()))
        })
}

#[cfg(test)]
mod tests {
    use plist::{Dictionary, Value};
    use rust_ios_protocol::OutputOptions;

    use super::*;

    fn invocation(path: &Path, extra: &[&str]) -> CommandInvocation {
        let mut argv = vec!["debug".into(), path.to_string_lossy().into_owned()];
        argv.extend(extra.iter().map(|value| (*value).to_owned()));
        CommandInvocation {
            argv,
            cwd: None,
            udid: Some("mock-phone".into()),
            output: OutputOptions::default(),
        }
    }

    fn write_app(directory: &Path) -> PathBuf {
        let app = directory.join("Demo.app");
        std::fs::create_dir(&app).unwrap();
        Value::Dictionary(Dictionary::from_iter([
            (
                "CFBundleIdentifier".to_owned(),
                Value::String("com.example.Demo".into()),
            ),
            (
                "CFBundleExecutable".to_owned(),
                Value::String("Demo".into()),
            ),
        ]))
        .to_file_xml(app.join("Info.plist"))
        .unwrap();
        std::fs::write(app.join("Demo"), b"mock executable").unwrap();
        app
    }

    #[test]
    fn parses_app_bundle_and_builds_mock_device_paths() {
        let directory = tempfile::tempdir().unwrap();
        let app = write_app(directory.path());
        let info = mock_debug_app_info(&invocation(&app, &[])).unwrap();
        assert_eq!(info.bundle_id, "com.example.Demo");
        assert!(info.local_executable_path.ends_with("Demo"));
        assert_eq!(
            info.device_executable_path,
            "/private/var/containers/Bundle/Application/MOCK-APP/Demo.app/Demo"
        );
    }

    #[test]
    fn rejects_missing_executable_and_unknown_options() {
        let directory = tempfile::tempdir().unwrap();
        let app = write_app(directory.path());
        std::fs::remove_file(app.join("Demo")).unwrap();
        assert!(matches!(
            local_debug_app(&invocation(&app, &[])),
            Err(CoreError::InvalidArgument(message)) if message.contains("executable does not exist")
        ));
        std::fs::write(app.join("Demo"), b"mock executable").unwrap();
        assert!(matches!(
            local_debug_app(&invocation(&app, &["--unknown"])),
            Err(CoreError::InvalidArgument(message)) if message.contains("does not accept option")
        ));
    }
}
