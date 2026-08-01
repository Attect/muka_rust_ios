use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use idevice::remote_pairing::{RemotePairingClient, RpPairingFile, connect_tls_psk_tunnel_native};
use idevice::tcp::adapter::Adapter;
use idevice::tcp::handle::AdapterHandle;
use idevice::{RemoteXpcClient, rsd::RsdHandshake};
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent};
use rust_ios_protocol::RemotePairingInfo;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::debug;
use uuid::Uuid;

use crate::{CoreError, CoreResult, UserSpaceTunnelInfo};

const REMOTED_SERVICE_TYPE: &str = "_remoted._tcp.local.";
const UNTRUSTED_TUNNEL_SERVICE: &str = "com.apple.internal.dt.coredevice.untrusted.tunnelservice";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const MANUAL_PAIR_TIMEOUT: Duration = Duration::from_secs(120);
const IDENTITY_FILENAME: &str = "selfIdentity.plist";
const SENDING_HOST: &str = "rust-ios";

#[derive(Debug)]
pub(crate) struct RemoteTunnelParts {
    pub info: UserSpaceTunnelInfo,
    pub adapter: AdapterHandle,
}

#[derive(Debug, Clone)]
struct RemoteEndpoint {
    address: ScopedIp,
    tunnel_service_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingFileFormat {
    RustIos,
    GoIos,
}

pub(crate) async fn pair_remote_tunnel(
    udid: &str,
    pair_record_dir: &Path,
) -> CoreResult<RemotePairingInfo> {
    let identity_path = identity_path(pair_record_dir)?;
    let existed = identity_path.is_file();
    let (mut pairing_file, format) = if existed {
        load_pairing_file(&identity_path).await?
    } else {
        (
            RpPairingFile::generate(SENDING_HOST),
            PairingFileFormat::RustIos,
        )
    };

    let endpoint = discover_remote_endpoint(udid).await?;
    let mut client = remote_pairing_client(&endpoint).await?;
    timeout(CONTROL_TIMEOUT, client.attempt_pair_verify())
        .await
        .map_err(|_| CoreError::Timeout(format!("RemotePairing handshake for {udid}")))?
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "RemotePairing handshake failed for {udid}: {error}"
            ))
        })?;
    let newly_paired = match timeout(CONTROL_TIMEOUT, client.validate_pairing(&mut pairing_file))
        .await
    {
        Ok(Ok(())) => false,
        Ok(Err(error)) if format == PairingFileFormat::GoIos => {
            return Err(CoreError::PairingRequired(format!(
                "go-ios RemotePairing identity {} was rejected for {udid}: {error}; use a separate writable --pair-record-path for manual pairing",
                identity_path.display()
            )));
        }
        Ok(Err(_)) => {
            timeout(
                MANUAL_PAIR_TIMEOUT,
                client.pair(&mut pairing_file, || async { "000000".to_owned() }),
            )
            .await
            .map_err(|_| {
                CoreError::Timeout(format!(
                    "RemotePairing manual consent for {udid} after {} seconds",
                    MANUAL_PAIR_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| {
                CoreError::PairingRequired(format!(
                    "RemotePairing manual pairing failed for {udid}: {error}"
                ))
            })?;
            true
        }
        Err(_) => {
            return Err(CoreError::Timeout(format!(
                "RemotePairing pair verification for {udid}"
            )));
        }
    };
    if !existed || newly_paired {
        write_pairing_file_atomic(&identity_path, &pairing_file)?;
    }

    Ok(RemotePairingInfo {
        udid: udid.to_owned(),
        identity_path: identity_path.to_string_lossy().into_owned(),
        device_address: endpoint.address.to_string(),
        newly_paired,
    })
}

pub(crate) async fn start_remote_tunnel(
    udid: &str,
    pair_record_dir: &Path,
) -> CoreResult<RemoteTunnelParts> {
    let identity_path = identity_path(pair_record_dir)?;
    let (mut pairing_file, _) = load_pairing_file(&identity_path).await?;
    let endpoint = discover_remote_endpoint(udid).await?;
    let mut client = remote_pairing_client(&endpoint).await?;

    timeout(CONTROL_TIMEOUT, client.attempt_pair_verify())
        .await
        .map_err(|_| CoreError::Timeout(format!("RemotePairing handshake for {udid}")))?
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "RemotePairing handshake failed for {udid}: {error}"
            ))
        })?;
    timeout(
        CONTROL_TIMEOUT,
        client.validate_pairing(&mut pairing_file),
    )
        .await
        .map_err(|_| CoreError::Timeout(format!("RemotePairing pair verification for {udid}")))?
        .map_err(|error| {
            CoreError::PairingRequired(format!(
                "stored RemotePairing identity was rejected for {udid}: {error}; run `ios tunnel pair --manual --udid {udid}`"
            ))
        })?;

    let encryption_key = client.encryption_key().to_vec();
    let tunnel_port = timeout(CONTROL_TIMEOUT, client.create_tcp_listener())
        .await
        .map_err(|_| CoreError::Timeout(format!("create RemotePairing TCP listener for {udid}")))?
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "create RemotePairing TCP listener for {udid}: {error}"
            ))
        })?;
    let stream = connect_endpoint(&endpoint.address, tunnel_port).await?;
    let tunnel = timeout(
        HANDSHAKE_TIMEOUT,
        connect_tls_psk_tunnel_native(stream, &encryption_key),
    )
    .await
    .map_err(|_| CoreError::Timeout("RemotePairing TLS-PSK/CDTunnel handshake".into()))?
    .map_err(|error| {
        CoreError::DeviceUnavailable(format!(
            "RemotePairing TLS-PSK/CDTunnel handshake failed for {udid}: {error}"
        ))
    })?;

    let tunnel_info = tunnel.info.clone();
    let client_address = tunnel_info
        .client_address
        .parse::<IpAddr>()
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "invalid RemotePairing client address {:?}: {error}",
                tunnel_info.client_address
            ))
        })?;
    let server_address = tunnel_info
        .server_address
        .parse::<IpAddr>()
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "invalid RemotePairing server address {:?}: {error}",
                tunnel_info.server_address
            ))
        })?;
    let mtu = usize::from(tunnel_info.mtu);
    let mut adapter = Adapter::new(
        Box::new(tunnel.into_inner()),
        client_address,
        server_address,
    );
    adapter.set_mss(mtu.saturating_sub(60));

    Ok(RemoteTunnelParts {
        info: UserSpaceTunnelInfo {
            udid: udid.to_owned(),
            client_address: tunnel_info.client_address,
            server_address: tunnel_info.server_address,
            netmask: tunnel_info.netmask,
            mtu: tunnel_info.mtu,
            rsd_port: tunnel_info.server_rsd_port,
        },
        adapter: adapter.to_async_handle(),
    })
}

async fn remote_pairing_client(
    endpoint: &RemoteEndpoint,
) -> CoreResult<RemotePairingClient<RemoteXpcClient<TcpStream>>> {
    let stream = connect_endpoint(&endpoint.address, endpoint.tunnel_service_port).await?;
    let mut xpc = timeout(CONTROL_TIMEOUT, RemoteXpcClient::new(stream))
        .await
        .map_err(|_| CoreError::Timeout("initialize RemotePairing RemoteXPC".into()))?
        .map_err(remote_device_error("initialize RemoteXPC"))?;
    timeout(CONTROL_TIMEOUT, xpc.do_handshake())
        .await
        .map_err(|_| CoreError::Timeout("perform RemotePairing RemoteXPC handshake".into()))?
        .map_err(remote_device_error("perform RemoteXPC handshake"))?;
    Ok(RemotePairingClient::new(xpc, SENDING_HOST))
}

async fn discover_remote_endpoint(udid: &str) -> CoreResult<RemoteEndpoint> {
    let services = discover_remoted_services().await?;
    let mut last_error = None;
    for service in services {
        let mut addresses = service.get_addresses().iter().collect::<Vec<_>>();
        addresses.sort_by_key(|address| !address.is_ipv6());
        for address in addresses {
            let stream = match connect_endpoint(address, service.get_port()).await {
                Ok(stream) => stream,
                Err(error) => {
                    last_error = Some(error.to_string());
                    continue;
                }
            };
            let handshake = match timeout(HANDSHAKE_TIMEOUT, RsdHandshake::new(stream)).await {
                Ok(Ok(handshake)) => handshake,
                Ok(Err(error)) => {
                    last_error = Some(error.to_string());
                    continue;
                }
                Err(_) => {
                    last_error = Some("external RSD handshake timed out".into());
                    continue;
                }
            };
            let candidate_udid = handshake
                .properties
                .get("UniqueDeviceID")
                .and_then(plist::Value::as_string);
            if candidate_udid != Some(udid) {
                debug!(
                    expected_udid = udid,
                    candidate_udid,
                    address = %address,
                    "ignoring external RSD endpoint for another device"
                );
                continue;
            }
            let tunnel_service_port = handshake
                .services
                .get(UNTRUSTED_TUNNEL_SERVICE)
                .map(|service| service.port)
                .ok_or_else(|| {
                    CoreError::DeviceUnavailable(format!(
                        "device {udid} external RSD does not advertise {UNTRUSTED_TUNNEL_SERVICE}"
                    ))
                })?;
            return Ok(RemoteEndpoint {
                address: address.clone(),
                tunnel_service_port,
            });
        }
    }

    let suffix = last_error
        .map(|error| format!("; last discovery error: {error}"))
        .unwrap_or_default();
    Err(CoreError::DeviceUnavailable(format!(
        "no {REMOTED_SERVICE_TYPE} endpoint matched device {udid}{suffix}"
    )))
}

async fn discover_remoted_services() -> CoreResult<Vec<ResolvedService>> {
    tokio::task::spawn_blocking(|| {
        let daemon = ServiceDaemon::new().map_err(|error| {
            CoreError::DeviceUnavailable(format!("initialize mDNS discovery: {error}"))
        })?;
        let receiver = daemon.browse(REMOTED_SERVICE_TYPE).map_err(|error| {
            CoreError::DeviceUnavailable(format!("browse {REMOTED_SERVICE_TYPE}: {error}"))
        })?;
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        let mut services = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match receiver.recv_timeout(remaining) {
                Ok(ServiceEvent::ServiceResolved(service)) => services.push(*service),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = daemon.stop_browse(REMOTED_SERVICE_TYPE);
        let _ = daemon.shutdown();
        if services.is_empty() {
            Err(CoreError::DeviceUnavailable(format!(
                "mDNS discovery found no {REMOTED_SERVICE_TYPE} service within {} seconds",
                DISCOVERY_TIMEOUT.as_secs()
            )))
        } else {
            Ok(services)
        }
    })
    .await
    .map_err(|error| CoreError::Backend(format!("mDNS discovery task failed: {error}")))?
}

async fn connect_endpoint(address: &ScopedIp, port: u16) -> CoreResult<TcpStream> {
    let socket = scoped_socket_addr(address, port).ok_or_else(|| {
        CoreError::DeviceUnavailable(format!("unsupported mDNS address {address}"))
    })?;
    timeout(CONNECT_TIMEOUT, TcpStream::connect(socket))
        .await
        .map_err(|_| CoreError::Timeout(format!("connect to RemotePairing endpoint {socket}")))?
        .map_err(|error| {
            CoreError::DeviceUnavailable(format!(
                "connect to RemotePairing endpoint {socket}: {error}"
            ))
        })
}

fn scoped_socket_addr(address: &ScopedIp, port: u16) -> Option<SocketAddr> {
    match address {
        ScopedIp::V4(address) => Some(SocketAddrV4::new(*address.addr(), port).into()),
        ScopedIp::V6(address) => {
            Some(SocketAddrV6::new(*address.addr(), port, 0, address.scope_id().index).into())
        }
        _ => None,
    }
}

fn identity_path(pair_record_dir: &Path) -> CoreResult<PathBuf> {
    if pair_record_dir.as_os_str().is_empty() {
        return Err(CoreError::InvalidArgument(
            "RemotePairing pair record path cannot be empty".into(),
        ));
    }
    if pair_record_dir.is_file() {
        return Err(CoreError::InvalidArgument(format!(
            "RemotePairing pair record path must be a directory: {}",
            pair_record_dir.display()
        )));
    }
    Ok(pair_record_dir.join(IDENTITY_FILENAME))
}

async fn load_pairing_file(path: &Path) -> CoreResult<(RpPairingFile, PairingFileFormat)> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CoreError::PermissionDenied(format!(
                "refusing to read RemotePairing identity through symlink {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CoreError::PairingRequired(format!(
                "RemotePairing identity is missing at {}; run `ios tunnel pair --manual` first",
                path.display()
            )));
        }
        Err(error) => return Err(error.into()),
    }
    let bytes = tokio::fs::read(path).await?;
    let mut dictionary = plist::from_bytes::<plist::Dictionary>(&bytes).map_err(|error| {
        CoreError::PairingRequired(format!(
            "cannot parse RemotePairing identity {}: {error}",
            path.display()
        ))
    })?;
    if !dictionary.contains_key("privateKey") && !dictionary.contains_key("publicKey") {
        let pairing_file = RpPairingFile::from_bytes(&bytes).map_err(|error| {
            CoreError::PairingRequired(format!(
                "cannot load RemotePairing identity {}: {error}",
                path.display()
            ))
        })?;
        return Ok((pairing_file, PairingFileFormat::RustIos));
    }
    let Some(private_key) = dictionary.remove("privateKey") else {
        return Err(CoreError::PairingRequired(format!(
            "RemotePairing identity {} uses an unsupported schema",
            path.display()
        )));
    };
    let Some(public_key) = dictionary.remove("publicKey") else {
        return Err(CoreError::PairingRequired(format!(
            "go-ios RemotePairing identity {} has no publicKey",
            path.display()
        )));
    };
    dictionary.insert("private_key".into(), private_key);
    dictionary.insert("public_key".into(), public_key);
    dictionary.remove("irk");
    let mut normalized = Vec::new();
    plist::to_writer_xml(&mut normalized, &dictionary).map_err(|error| {
        CoreError::PairingRequired(format!(
            "cannot normalize go-ios RemotePairing identity {}: {error}",
            path.display()
        ))
    })?;
    let pairing_file = RpPairingFile::from_bytes(&normalized).map_err(|error| {
        CoreError::PairingRequired(format!(
            "cannot load go-ios RemotePairing identity {}: {error}",
            path.display()
        ))
    })?;
    Ok((pairing_file, PairingFileFormat::GoIos))
}

fn write_pairing_file_atomic(path: &Path, pairing_file: &RpPairingFile) -> CoreResult<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(CoreError::PermissionDenied(format!(
            "refusing to replace RemotePairing identity symlink {}",
            path.display()
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        CoreError::InvalidArgument(format!(
            "RemotePairing identity has no parent directory: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{IDENTITY_FILENAME}.tmp-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&pairing_file.to_bytes())?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temporary, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
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
    // SAFETY: 两个 UTF-16 路径都以 NUL 结尾，并在调用期间保持有效。
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
    std::fs::rename(source, target)
}

fn remote_device_error(context: &'static str) -> impl FnOnce(idevice::IdeviceError) -> CoreError {
    move |error| CoreError::DeviceUnavailable(format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pairing_file_round_trip_and_atomic_replace() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILENAME);
        let first = RpPairingFile::generate("first-host");
        write_pairing_file_atomic(&path, &first).unwrap();
        let (loaded, format) = load_pairing_file(&path).await.unwrap();
        assert_eq!(format, PairingFileFormat::RustIos);
        assert_eq!(loaded.identifier(), first.identifier());
        assert_eq!(loaded.private_key_bytes(), first.private_key_bytes());

        let second = RpPairingFile::generate("second-host");
        write_pairing_file_atomic(&path, &second).unwrap();
        let (loaded, format) = load_pairing_file(&path).await.unwrap();
        assert_eq!(format, PairingFileFormat::RustIos);
        assert_eq!(loaded.identifier(), second.identifier());
        assert_eq!(loaded.private_key_bytes(), second.private_key_bytes());
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "temporary identity file must not remain"
        );
    }

    #[tokio::test]
    async fn missing_identity_is_structured_pairing_required() {
        let directory = tempfile::tempdir().unwrap();
        let error = load_pairing_file(&directory.path().join(IDENTITY_FILENAME))
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::PairingRequired(_)));
    }

    #[tokio::test]
    async fn go_ios_identity_schema_is_loaded_without_rewrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(IDENTITY_FILENAME);
        let original = RpPairingFile::generate("go-ios-host");
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert(
            "identifier".into(),
            plist::Value::String(original.identifier().into()),
        );
        dictionary.insert(
            "privateKey".into(),
            plist::Value::Data(original.private_key_bytes()),
        );
        dictionary.insert(
            "publicKey".into(),
            plist::Value::Data(original.public_key_bytes()),
        );
        dictionary.insert("irk".into(), plist::Value::Data(vec![7; 16]));
        let legacy = plist::Value::Dictionary(dictionary);
        legacy.to_file_binary(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let (loaded, format) = load_pairing_file(&path).await.unwrap();
        assert_eq!(format, PairingFileFormat::GoIos);
        assert_eq!(loaded.identifier(), original.identifier());
        assert_eq!(loaded.private_key_bytes(), original.private_key_bytes());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn pair_record_path_must_be_a_directory() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("identity.plist");
        std::fs::write(&file, b"x").unwrap();
        assert!(matches!(
            identity_path(&file),
            Err(CoreError::InvalidArgument(_))
        ));
    }
}
