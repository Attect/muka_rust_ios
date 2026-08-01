use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::platform::atomic_replace;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerMetadata {
    pub schema_version: u32,
    pub pid: u32,
    pub instance_id: Uuid,
    pub server_version: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub listen_addr: SocketAddr,
    pub started_at_ms: u64,
    pub executable: PathBuf,
}

impl ServerMetadata {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(path)?;
        let metadata: Self = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid server metadata {}: {error}", path.display()),
            )
        })?;
        if metadata.schema_version != Self::SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported server metadata schema {}",
                    metadata.schema_version
                ),
            ));
        }
        Ok(metadata)
    }

    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(path, &bytes)
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trip_and_replace() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.json");
        let mut metadata = ServerMetadata {
            schema_version: ServerMetadata::SCHEMA_VERSION,
            pid: 42,
            instance_id: Uuid::new_v4(),
            server_version: "0.1.0".into(),
            protocol_major: 1,
            protocol_minor: 0,
            listen_addr: "127.0.0.1:12345".parse().unwrap(),
            started_at_ms: 123,
            executable: "ios.exe".into(),
        };
        metadata.write_atomic(&path).unwrap();
        assert_eq!(ServerMetadata::read(&path).unwrap(), metadata);
        metadata.pid = 43;
        metadata.write_atomic(&path).unwrap();
        assert_eq!(ServerMetadata::read(&path).unwrap().pid, 43);
    }
}
