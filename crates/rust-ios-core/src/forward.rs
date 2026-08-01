use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{BoxedIo, CoreResult};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BridgeStats {
    pub left_to_right: u64,
    pub right_to_left: u64,
    pub cancelled: bool,
}

/// 在两个数据面连接之间复制数据。任何一端关闭或 task 被取消都会同时关闭两端。
pub async fn bridge_streams(
    mut left: BoxedIo,
    mut right: BoxedIo,
    cancel: CancellationToken,
) -> CoreResult<BridgeStats> {
    tokio::select! {
        result = tokio::io::copy_bidirectional(&mut left, &mut right) => {
            let (left_to_right, right_to_left) = result?;
            let _ = left.shutdown().await;
            let _ = right.shutdown().await;
            Ok(BridgeStats { left_to_right, right_to_left, cancelled: false })
        }
        () = cancel.cancelled() => {
            let _ = left.shutdown().await;
            let _ = right.shutdown().await;
            Ok(BridgeStats { cancelled: true, ..BridgeStats::default() })
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn bridge_copies_both_directions() {
        let (mut left_peer, left_bridge) = tokio::io::duplex(1024);
        let (mut right_peer, right_bridge) = tokio::io::duplex(1024);
        let token = CancellationToken::new();
        let task = tokio::spawn(bridge_streams(
            Box::new(left_bridge),
            Box::new(right_bridge),
            token,
        ));

        left_peer.write_all(b"left").await.unwrap();
        let mut from_left = [0_u8; 4];
        right_peer.read_exact(&mut from_left).await.unwrap();
        assert_eq!(&from_left, b"left");

        right_peer.write_all(b"right").await.unwrap();
        let mut from_right = [0_u8; 5];
        left_peer.read_exact(&mut from_right).await.unwrap();
        assert_eq!(&from_right, b"right");

        drop(left_peer);
        drop(right_peer);
        let stats = task.await.unwrap().unwrap();
        assert_eq!(stats.left_to_right, 4);
        assert_eq!(stats.right_to_left, 5);
    }

    #[tokio::test]
    async fn cancellation_stops_bridge() {
        let (_left_peer, left_bridge) = tokio::io::duplex(16);
        let (_right_peer, right_bridge) = tokio::io::duplex(16);
        let token = CancellationToken::new();
        let child = token.clone();
        let task = tokio::spawn(bridge_streams(
            Box::new(left_bridge),
            Box::new(right_bridge),
            child,
        ));
        token.cancel();
        assert!(task.await.unwrap().unwrap().cancelled);
    }
}
