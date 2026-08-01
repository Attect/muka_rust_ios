use std::io;
use std::path::{Path, PathBuf};

use rust_ios_protocol::TaskRecord;
use serde::{Deserialize, Serialize};

use crate::metadata::write_atomic;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PersistedState {
    pub schema_version: u32,
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
}

impl PersistedState {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn empty() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<PersistedState> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let state: PersistedState = serde_json::from_slice(&bytes).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid state {}: {error}", self.path.display()),
                    )
                })?;
                if state.schema_version != PersistedState::SCHEMA_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported state schema {}", state.schema_version),
                    ));
                }
                Ok(state)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PersistedState::empty()),
            Err(error) => Err(error),
        }
    }

    pub fn load_or_quarantine(&self) -> io::Result<PersistedState> {
        match self.load() {
            Ok(state) => Ok(state),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let quarantine = self
                    .path
                    .with_extension(format!("corrupt-{}", crate::runtime::unix_time_ms()));
                std::fs::rename(&self.path, &quarantine)?;
                tracing::error!(
                    state = %self.path.display(),
                    quarantine = %quarantine.display(),
                    %error,
                    "corrupt task state quarantined"
                );
                Ok(PersistedState::empty())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, state: &PersistedState) -> io::Result<()> {
        if state.schema_version != PersistedState::SCHEMA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to write unsupported state schema",
            ));
        }
        let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
        write_atomic(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_loads_empty() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path().join("state.json"));
        assert_eq!(store.load().unwrap(), PersistedState::empty());
    }

    #[test]
    fn corrupt_state_is_quarantined() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        std::fs::write(&path, "{broken").unwrap();
        let store = StateStore::new(&path);
        assert_eq!(store.load_or_quarantine().unwrap(), PersistedState::empty());
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            1,
            "quarantine file should remain"
        );
    }
}
