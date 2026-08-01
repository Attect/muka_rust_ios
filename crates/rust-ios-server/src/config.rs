use std::env;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::BaseDirs;

pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:60106";
pub const HOME_ENV: &str = "RUST_IOS_HOME";
pub const SERVER_ADDR_ENV: &str = "RUST_IOS_SERVER_ADDR";

#[derive(Debug, Clone)]
pub struct ServerPaths {
    pub root: PathBuf,
    pub lock: PathBuf,
    pub token: PathBuf,
    pub metadata: PathBuf,
    pub state: PathBuf,
    pub log: PathBuf,
}

impl ServerPaths {
    pub fn discover() -> io::Result<Self> {
        let root = match env::var_os(HOME_ENV) {
            Some(value) => PathBuf::from(value),
            None => BaseDirs::new()
                .ok_or_else(|| io::Error::other("unable to determine local data directory"))?
                .data_local_dir()
                .join("rust-ios"),
        };
        Ok(Self::new(root))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            lock: root.join("server.lock"),
            token: root.join("auth.token"),
            metadata: root.join("server.json"),
            state: root.join("state-v1.json"),
            log: root.join("server.log"),
            root,
        }
    }

    pub fn ensure(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)
    }

    pub fn display_root(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }

    pub fn is_within_root(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    pub paths: ServerPaths,
    pub handshake_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub device_poll_interval: Duration,
}

impl ServerConfig {
    pub fn discover() -> io::Result<Self> {
        Self::discover_with_paths(ServerPaths::discover()?)
    }

    pub fn discover_with_paths(paths: ServerPaths) -> io::Result<Self> {
        let raw_addr = env::var(SERVER_ADDR_ENV).unwrap_or_else(|_| DEFAULT_SERVER_ADDR.into());
        let listen_addr = raw_addr.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {SERVER_ADDR_ENV} value {raw_addr:?}: {error}"),
            )
        })?;
        Ok(Self {
            listen_addr,
            paths,
            handshake_timeout: Duration::from_secs(3),
            shutdown_timeout: Duration::from_secs(5),
            device_poll_interval: Duration::from_secs(1),
        })
    }

    pub fn isolated(root: impl Into<PathBuf>, listen_addr: SocketAddr) -> Self {
        Self {
            listen_addr,
            paths: ServerPaths::new(root),
            handshake_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
            device_poll_interval: Duration::from_millis(100),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_runtime_paths_stay_under_root() {
        let paths = ServerPaths::new("C:/tmp/rust-ios-test");
        for path in [
            &paths.lock,
            &paths.token,
            &paths.metadata,
            &paths.state,
            &paths.log,
        ] {
            assert!(paths.is_within_root(path));
        }
    }
}
