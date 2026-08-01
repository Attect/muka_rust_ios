use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard upper bound checked before allocating a frame payload.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("I/O error while processing frame: {0}")]
    Io(#[from] std::io::Error),
    #[error("zero-length frames are not allowed")]
    ZeroLength,
    #[error("frame length {actual} exceeds maximum {maximum}")]
    TooLarge { actual: usize, maximum: usize },
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
}

pub async fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() {
        return Err(FrameError::ZeroLength);
    }
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            actual: payload.len(),
            maximum: MAX_FRAME_LEN,
        });
    }

    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_json_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(FrameError::ZeroLength);
    }
    if length > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            actual: length,
            maximum: MAX_FRAME_LEN,
        });
    }

    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Example {
        value: String,
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut client, mut server) = duplex(1024);
        let expected = Example {
            value: "你好, iOS".to_owned(),
        };

        let send = tokio::spawn(async move { write_json_frame(&mut client, &expected).await });
        let actual: Example = read_json_frame(&mut server).await.expect("read frame");
        send.await.expect("join writer").expect("write frame");
        assert_eq!(actual.value, "你好, iOS");
    }

    #[tokio::test]
    async fn rejects_zero_length_before_json_decode() {
        let (mut client, mut server) = duplex(16);
        client.write_all(&0_u32.to_be_bytes()).await.unwrap();
        let err = read_json_frame::<_, Example>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::ZeroLength));
    }

    #[tokio::test]
    async fn rejects_oversized_frame_before_allocation() {
        let (mut client, mut server) = duplex(16);
        client
            .write_all(&((MAX_FRAME_LEN as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_json_frame::<_, Example>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn reports_truncated_payload() {
        let (mut client, mut server) = duplex(32);
        client.write_all(&10_u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{}").await.unwrap();
        client.shutdown().await.unwrap();
        let err = read_json_frame::<_, Example>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(err, FrameError::Io(_)));
    }
}
