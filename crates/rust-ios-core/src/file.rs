use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileDomain {
    AppData,
    AppGroup,
    Temporary,
    SystemCrashLogs,
}

impl FileDomain {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::AppData => "app_data",
            Self::AppGroup => "app_group",
            Self::Temporary => "temporary",
            Self::SystemCrashLogs => "system_crash_logs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileServiceSelection {
    pub(crate) domain: FileDomain,
    pub(crate) identifier: String,
}

impl FileServiceSelection {
    pub(crate) fn requires_tunnel(&self) -> bool {
        self.domain != FileDomain::AppData
    }
}

pub(crate) fn has_domain_selector(arguments: &[String]) -> bool {
    ["--app", "--app-group", "--crash", "--temp"]
        .iter()
        .any(|name| option_is_present(arguments, name))
}

pub(crate) fn parse_selection(arguments: &[String]) -> CoreResult<Option<FileServiceSelection>> {
    let app = optional_non_empty_value(arguments, "--app")?;
    let app_group = optional_non_empty_value(arguments, "--app-group")?;
    let crash = flag_is_present(arguments, "--crash");
    let temp = flag_is_present(arguments, "--temp");
    let count = usize::from(app.is_some())
        + usize::from(app_group.is_some())
        + usize::from(crash)
        + usize::from(temp);
    if count > 1 {
        return Err(CoreError::InvalidArgument(
            "file accepts only one of --app, --app-group, --crash or --temp".into(),
        ));
    }
    Ok(if let Some(identifier) = app {
        Some(FileServiceSelection {
            domain: FileDomain::AppData,
            identifier,
        })
    } else if let Some(identifier) = app_group {
        Some(FileServiceSelection {
            domain: FileDomain::AppGroup,
            identifier,
        })
    } else if crash {
        Some(FileServiceSelection {
            domain: FileDomain::SystemCrashLogs,
            identifier: String::new(),
        })
    } else if temp {
        Some(FileServiceSelection {
            domain: FileDomain::Temporary,
            identifier: String::new(),
        })
    } else {
        None
    })
}

fn option_is_present(arguments: &[String], name: &str) -> bool {
    let prefix = format!("{name}=");
    arguments
        .iter()
        .any(|value| value == name || value.starts_with(&prefix))
}

fn flag_is_present(arguments: &[String], name: &str) -> bool {
    arguments.iter().any(|value| value == name)
}

fn optional_non_empty_value(arguments: &[String], name: &str) -> CoreResult<Option<String>> {
    let prefix = format!("{name}=");
    for (index, argument) in arguments.iter().enumerate() {
        let value = if let Some(value) = argument.strip_prefix(&prefix) {
            Some(value)
        } else if argument == name {
            arguments.get(index + 1).map(String::as_str)
        } else {
            None
        };
        if let Some(value) = value {
            if value.is_empty() || value.starts_with('-') {
                return Err(CoreError::InvalidArgument(format!(
                    "file {name} requires a non-empty identifier"
                )));
            }
            return Ok(Some(value.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(feature = "real-device")]
mod real {
    use std::future::Future;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::time::Duration;

    use bytes::Bytes;
    use ios_device_core::XpcClient;
    use ios_device_core::fileservice::{
        Domain, FileServiceClient, FileServiceError, FileWriteOptions, MAX_FILE_SIZE,
        MAX_INLINE_DATA_SIZE,
    };
    use serde_json::{Value, json};
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

    use super::{FileDomain, FileServiceSelection};
    use crate::{
        CoreError, CoreResult, RsdServiceKind, UserSpaceTunnel, real::resolve_client_path,
    };
    use rust_ios_protocol::CommandInvocation;

    const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

    pub(crate) fn execute<'a>(
        invocation: &'a CommandInvocation,
        tunnel: &'a dyn UserSpaceTunnel,
        selection: FileServiceSelection,
    ) -> Pin<Box<dyn Future<Output = CoreResult<Value>> + Send + 'a>> {
        Box::pin(execute_inner(invocation, tunnel, selection))
    }

    async fn execute_inner(
        invocation: &CommandInvocation,
        tunnel: &dyn UserSpaceTunnel,
        selection: FileServiceSelection,
    ) -> CoreResult<Value> {
        tunnel
            .ensure_rsd_service(RsdServiceKind::FileServiceControl)
            .await?;
        tunnel
            .ensure_rsd_service(RsdServiceKind::FileServiceData)
            .await?;
        let control_stream = tokio::time::timeout(
            CONTROL_TIMEOUT,
            tunnel.connect_rsd_service(RsdServiceKind::FileServiceControl),
        )
        .await
        .map_err(|_| timeout_error("connect control service"))??;
        let control = tokio::time::timeout(
            CONTROL_TIMEOUT,
            Box::pin(XpcClient::connect_stream(control_stream)),
        )
        .await
        .map_err(|_| timeout_error("control XPC handshake"))?
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "FileService control XPC handshake failed: {error}"
            ))
        })?;
        let mut client = tokio::time::timeout(
            CONTROL_TIMEOUT,
            Box::pin(FileServiceClient::connect(
                control,
                protocol_domain(selection.domain),
                &selection.identifier,
            )),
        )
        .await
        .map_err(|_| timeout_error("create session"))?
        .map_err(|error| map_error("create session", error))?;

        let action =
            invocation.argv.get(1).map(String::as_str).ok_or_else(|| {
                CoreError::InvalidArgument("file requires ls, pull or push".into())
            })?;
        match action {
            "ls" => list(&mut client, invocation, &selection).await,
            "pull" => pull(&mut client, invocation, tunnel, &selection).await,
            "push" => push(&mut client, invocation, tunnel, &selection).await,
            _ => Err(CoreError::InvalidArgument(format!(
                "unknown file action {action:?}"
            ))),
        }
    }

    async fn list(
        client: &mut FileServiceClient,
        invocation: &CommandInvocation,
        selection: &FileServiceSelection,
    ) -> CoreResult<Value> {
        let path = option_value(&invocation.argv, "--path")?.unwrap_or_else(|| ".".to_owned());
        validate_remote_path(&path, "file ls --path")?;
        let mut files =
            tokio::time::timeout(CONTROL_TIMEOUT, Box::pin(client.list_directory(&path)))
                .await
                .map_err(|_| timeout_error("list directory"))?
                .map_err(|error| map_error("list directory", error))?;
        files.sort();
        let count = files.len();
        Ok(json!({
            "path": path,
            "files": files,
            "count": count,
            "domain": selection.domain.label(),
            "identifier": optional_identifier(selection),
            "serviceTransport": "shared_rsd",
        }))
    }

    async fn pull(
        client: &mut FileServiceClient,
        invocation: &CommandInvocation,
        tunnel: &dyn UserSpaceTunnel,
        selection: &FileServiceSelection,
    ) -> CoreResult<Value> {
        let remote = required_option(&invocation.argv, "--remote", "file pull")?;
        validate_remote_path(&remote, "file pull --remote")?;
        let raw_local = required_option(&invocation.argv, "--local", "file pull")?;
        let output = resolve_client_path(invocation, &raw_local);
        let temporary = temporary_output_path(&output)?;
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let transfer = download_to_output(client, &remote, tunnel, &temporary, &output).await;
        let bytes = match transfer {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error);
            }
        };
        Ok(json!({
            "remote": remote,
            "local": output,
            "size": bytes,
            "bytes": bytes,
            "domain": selection.domain.label(),
            "identifier": optional_identifier(selection),
            "serviceTransport": "shared_rsd",
        }))
    }

    fn download_to_output<'a>(
        client: &'a mut FileServiceClient,
        remote: &'a str,
        tunnel: &'a dyn UserSpaceTunnel,
        temporary: &'a Path,
        output: &'a Path,
    ) -> Pin<Box<dyn Future<Output = CoreResult<u64>> + Send + 'a>> {
        Box::pin(async move {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temporary)
                .await?;
            let mut data_stream = tokio::time::timeout(
                CONTROL_TIMEOUT,
                tunnel.connect_rsd_service(RsdServiceKind::FileServiceData),
            )
            .await
            .map_err(|_| timeout_error("connect data service"))??;
            let bytes =
                Box::pin(client.download_file_to_writer(remote, &mut data_stream, &mut file))
                    .await
                    .map_err(|error| map_error("download file", error))?;
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            atomic_replace(temporary, output)?;
            Ok(bytes)
        })
    }

    async fn push(
        client: &mut FileServiceClient,
        invocation: &CommandInvocation,
        tunnel: &dyn UserSpaceTunnel,
        selection: &FileServiceSelection,
    ) -> CoreResult<Value> {
        let raw_local = required_option(&invocation.argv, "--local", "file push")?;
        let input = resolve_client_path(invocation, &raw_local);
        let remote = required_option(&invocation.argv, "--remote", "file push")?;
        validate_remote_path(&remote, "file push --remote")?;
        let metadata = tokio::fs::metadata(&input).await.map_err(|error| {
            CoreError::Io(io::Error::new(
                error.kind(),
                format!("cannot read {}: {error}", input.display()),
            ))
        })?;
        if !metadata.is_file() {
            return Err(CoreError::InvalidArgument(format!(
                "file push source {} is not a file",
                input.display()
            )));
        }
        let size = metadata.len();
        if size > MAX_FILE_SIZE {
            return Err(CoreError::InvalidArgument(format!(
                "file push source is {size} bytes; FileService limit is {MAX_FILE_SIZE} bytes"
            )));
        }
        let options = FileWriteOptions {
            permissions: file_permissions(&metadata),
            ..FileWriteOptions::default()
        };
        if size <= MAX_INLINE_DATA_SIZE {
            let bytes = tokio::fs::read(&input).await?;
            Box::pin(client.upload_inline_file(&remote, Bytes::from(bytes), options))
                .await
                .map_err(|error| map_error("upload inline file", error))?;
        } else {
            let ticket = Box::pin(client.propose_file_upload(&remote, size, options))
                .await
                .map_err(|error| map_error("propose file upload", error))?;
            let mut data_stream = tokio::time::timeout(
                CONTROL_TIMEOUT,
                tunnel.connect_rsd_service(RsdServiceKind::FileServiceData),
            )
            .await
            .map_err(|_| timeout_error("connect data service"))??;
            let mut file = tokio::fs::File::open(&input).await?;
            Box::pin(client.upload_file_data(&mut data_stream, &ticket, &mut file, size))
                .await
                .map_err(|error| map_error("upload file data", error))?;
        }
        Ok(json!({
            "remote": remote,
            "local": input,
            "size": size,
            "bytes": size,
            "domain": selection.domain.label(),
            "identifier": optional_identifier(selection),
            "serviceTransport": "shared_rsd",
        }))
    }

    fn protocol_domain(domain: FileDomain) -> Domain {
        match domain {
            FileDomain::AppData => Domain::AppDataContainer,
            FileDomain::AppGroup => Domain::AppGroupDataContainer,
            FileDomain::Temporary => Domain::Temporary,
            FileDomain::SystemCrashLogs => Domain::SystemCrashLogs,
        }
    }

    fn optional_identifier(selection: &FileServiceSelection) -> Option<&str> {
        (!selection.identifier.is_empty()).then_some(selection.identifier.as_str())
    }

    fn required_option(arguments: &[String], name: &str, context: &str) -> CoreResult<String> {
        option_value(arguments, name)?
            .ok_or_else(|| CoreError::InvalidArgument(format!("{context} requires {name}")))
    }

    fn option_value(arguments: &[String], name: &str) -> CoreResult<Option<String>> {
        let prefix = format!("{name}=");
        for (index, argument) in arguments.iter().enumerate() {
            let value = if let Some(value) = argument.strip_prefix(&prefix) {
                Some(value)
            } else if argument == name {
                arguments.get(index + 1).map(String::as_str)
            } else {
                None
            };
            if let Some(value) = value {
                if value.is_empty() || value.starts_with('-') {
                    return Err(CoreError::InvalidArgument(format!(
                        "{name} requires a non-empty value"
                    )));
                }
                return Ok(Some(value.to_owned()));
            }
        }
        Ok(None)
    }

    fn validate_remote_path(path: &str, context: &str) -> CoreResult<()> {
        if path.is_empty() || path.contains('\0') {
            return Err(CoreError::InvalidArgument(format!(
                "{context} must be a non-empty path without NUL bytes"
            )));
        }
        Ok(())
    }

    fn temporary_output_path(output: &Path) -> CoreResult<PathBuf> {
        let name = output
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                CoreError::InvalidArgument(format!(
                    "file pull output {} has no valid UTF-8 file name",
                    output.display()
                ))
            })?;
        Ok(output.with_file_name(format!(".{name}.rust-ios-{}.part", Uuid::new_v4())))
    }

    pub(super) fn map_error(context: &str, error: FileServiceError) -> CoreError {
        match error {
            FileServiceError::Io(error) => CoreError::Io(io::Error::new(
                error.kind(),
                format!("FileService {context}: {error}"),
            )),
            FileServiceError::Xpc(error) => {
                CoreError::DeviceUnavailable(format!("FileService {context} XPC failed: {error}"))
            }
            FileServiceError::Protocol(message)
                if message
                    .to_ascii_lowercase()
                    .contains("containerlookuperrordomain") =>
            {
                CoreError::InvalidArgument(format!("FileService {context}: {message}"))
            }
            FileServiceError::Protocol(message)
                if message.to_ascii_lowercase().contains("permission") =>
            {
                CoreError::PermissionDenied(format!("FileService {context}: {message}"))
            }
            FileServiceError::Protocol(message) => {
                CoreError::Backend(format!("FileService {context}: {message}"))
            }
        }
    }

    fn timeout_error(context: &str) -> CoreError {
        CoreError::DeviceUnavailable(format!(
            "FileService {context} timed out after {} seconds",
            CONTROL_TIMEOUT.as_secs()
        ))
    }

    #[cfg(unix)]
    fn file_permissions(metadata: &std::fs::Metadata) -> i64 {
        use std::os::unix::fs::PermissionsExt;
        i64::from(metadata.permissions().mode() & 0o777)
    }

    #[cfg(not(unix))]
    fn file_permissions(_metadata: &std::fs::Metadata) -> i64 {
        0o644
    }

    #[cfg(windows)]
    fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };

        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: 两个 UTF-16 buffer 均以 NUL 结尾，并在调用期间保持有效。
        let success = unsafe {
            MoveFileExW(
                source.as_ptr(),
                target.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(windows))]
    fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
        std::fs::rename(source, target)
    }
}

#[cfg(feature = "real-device")]
pub(crate) use real::execute;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_fileservice_domains() {
        for (arguments, domain, identifier) in [
            (
                vec!["file", "ls", "--app=com.example.App"],
                FileDomain::AppData,
                "com.example.App",
            ),
            (
                vec!["file", "ls", "--app-group", "group.example.shared"],
                FileDomain::AppGroup,
                "group.example.shared",
            ),
            (
                vec!["file", "ls", "--crash"],
                FileDomain::SystemCrashLogs,
                "",
            ),
            (vec!["file", "ls", "--temp"], FileDomain::Temporary, ""),
        ] {
            let arguments: Vec<String> = arguments.into_iter().map(str::to_owned).collect();
            let selection = parse_selection(&arguments).unwrap().unwrap();
            assert_eq!(selection.domain, domain);
            assert_eq!(selection.identifier, identifier);
        }
    }

    #[test]
    fn rejects_ambiguous_or_empty_domains() {
        let ambiguous = ["file", "ls", "--app=a", "--temp"].map(str::to_owned);
        assert!(matches!(
            parse_selection(&ambiguous),
            Err(CoreError::InvalidArgument(_))
        ));
        let empty = ["file", "ls", "--app-group="].map(str::to_owned);
        assert!(matches!(
            parse_selection(&empty),
            Err(CoreError::InvalidArgument(_))
        ));
    }

    #[cfg(feature = "real-device")]
    #[test]
    fn maps_missing_app_or_group_container_to_invalid_argument() {
        let error = real::map_error(
            "create session",
            ios_device_core::fileservice::FileServiceError::Protocol(
                "The requested container could not be found (ContainerLookupErrorDomain error 2.)"
                    .into(),
            ),
        );
        assert!(matches!(
            error,
            CoreError::InvalidArgument(message)
                if message.contains("ContainerLookupErrorDomain")
        ));
    }

    #[cfg(feature = "real-device")]
    #[tokio::test]
    async fn fileservice_execute_future_stays_below_windows_stack_budget() {
        use std::mem::size_of_val;

        use rust_ios_protocol::{CommandInvocation, OutputOptions, TunnelMode, TunnelSpec};

        use crate::{DeviceBackend, MockDeviceBackend};

        let backend = MockDeviceBackend::new();
        backend.attach_usb("mock-phone").await;
        let tunnel = backend
            .start_userspace_tunnel(&TunnelSpec {
                udid: Some("mock-phone".into()),
                mode: TunnelMode::Userspace,
                transport: rust_ios_protocol::TunnelTransport::UsbCoreDevice,
                pair_record_path: "default".into(),
                userspace_bind_host: "127.0.0.1".into(),
                userspace_port: 0,
                kernel_interface_name: None,
                wintun_path: None,
            })
            .await
            .unwrap();
        let invocation = CommandInvocation {
            argv: vec!["file".into(), "ls".into(), "--temp".into()],
            cwd: None,
            udid: Some("mock-phone".into()),
            output: OutputOptions::default(),
        };
        let selection = parse_selection(&invocation.argv).unwrap().unwrap();
        let future = execute(&invocation, tunnel.as_ref(), selection);
        let size = size_of_val(&future);
        eprintln!("FileService execute future size: {size} bytes");
        assert!(size < 64, "boxed FileService future is {size} bytes");
    }
}
