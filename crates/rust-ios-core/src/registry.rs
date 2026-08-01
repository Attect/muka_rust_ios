use std::sync::Arc;
use std::time::Duration;

use rust_ios_protocol::DeviceList;
use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{CoreResult, DeviceBackend};

#[derive(Debug)]
pub struct DeviceRegistry {
    backend: Arc<dyn DeviceBackend>,
    snapshot: RwLock<DeviceList>,
    changes: watch::Sender<DeviceList>,
}

impl DeviceRegistry {
    pub fn new(backend: Arc<dyn DeviceBackend>) -> Self {
        let initial = DeviceList::default();
        let (changes, _) = watch::channel(initial.clone());
        Self {
            backend,
            snapshot: RwLock::new(initial),
            changes,
        }
    }

    pub fn backend(&self) -> &Arc<dyn DeviceBackend> {
        &self.backend
    }

    pub async fn snapshot(&self) -> DeviceList {
        self.snapshot.read().await.clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<DeviceList> {
        self.changes.subscribe()
    }

    pub async fn refresh(&self) -> CoreResult<DeviceList> {
        let next = self.backend.list_devices().await?;
        let mut current = self.snapshot.write().await;
        if *current != next {
            *current = next.clone();
            self.changes.send_replace(next.clone());
        }
        Ok(next)
    }

    pub async fn run(&self, interval: Duration, cancel: CancellationToken) {
        loop {
            match self.refresh().await {
                Ok(list) => debug!(
                    device_count = list.devices.len(),
                    "device snapshot refreshed"
                ),
                Err(error) => warn!(%error, "failed to refresh device snapshot"),
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockDeviceBackend;

    #[tokio::test]
    async fn registry_publishes_attach_and_detach() {
        let backend = MockDeviceBackend::new();
        let registry = DeviceRegistry::new(Arc::new(backend.clone()));
        let mut receiver = registry.subscribe();

        backend.attach_usb("mock-a").await;
        registry.refresh().await.unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(receiver.borrow().devices[0].udid, "mock-a");

        backend.detach("mock-a").await;
        registry.refresh().await.unwrap();
        receiver.changed().await.unwrap();
        assert!(receiver.borrow().devices.is_empty());
    }
}
