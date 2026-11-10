// Jackson Coxson

use async_stream::try_stream;
use futures::Stream;
use http2::Setting;
use tracing::debug;

use crate::{CdTunnelError, IdeviceError, ReadWrite, xpc};

pub mod errors;
mod format;
mod http2;
pub mod xpc_macro;

use format::XPCFlag;
pub use format::{Dictionary, XPCMessage, XPCObject};
use format::{MAX_XPC_MESSAGE_SIZE, XPC_MESSAGE_WRAPPER_LEN};

const ROOT_CHANNEL: u32 = 1;
const REPLY_CHANNEL: u32 = 3;

/// Fixed XPC message-wrapper header: magic + flags + body length + message id.
const XPC_WRAPPER_LEN: usize = XPC_MESSAGE_WRAPPER_LEN;

#[derive(Debug)]
pub struct RemoteXpcClient<R: ReadWrite> {
    h2_client: http2::Http2Client<R>,
    root_id: u64,
    // reply_id: u64 // maybe not used?
    /// Per-channel bytes accumulated toward the next whole XPC message. Persisted
    /// across `recv_from_channel` calls so a partially-received message survives a
    /// cancelled read — required for `recv_push` to be safe in a `select!`.
    partial: std::collections::HashMap<u32, Vec<u8>>,
}

impl<R: ReadWrite> RemoteXpcClient<R> {
    pub async fn new(socket: R) -> Result<Self, IdeviceError> {
        Ok(Self {
            h2_client: http2::Http2Client::new(socket).await?,
            root_id: 1,
            partial: std::collections::HashMap::new(),
        })
    }

    pub async fn do_handshake(&mut self) -> Result<(), IdeviceError> {
        self.h2_client
            .set_settings(
                vec![
                    Setting::MaxConcurrentStreams(100),
                    Setting::InitialWindowSize(1048576),
                ],
                0,
            )
            .await?;
        self.h2_client.window_update(983041, 0).await?;
        self.h2_client.open_stream(1).await?; // root channel

        debug!("Sending empty dictionary");
        self.send_root(XPCMessage::new(
            Some(XPCFlag::AlwaysSet),
            Some(XPCObject::Dictionary(Default::default())),
            None,
        ))
        .await?;

        debug!("Opening reply stream");
        self.h2_client.open_stream(REPLY_CHANNEL).await?;
        self.send_reply(XPCMessage::new(
            Some(XPCFlag::InitHandshake | XPCFlag::AlwaysSet),
            None,
            None,
        ))
        .await?;

        debug!("Sending weird flags");
        self.send_root(XPCMessage::new(Some(XPCFlag::Custom(0x201)), None, None))
            .await?;

        Ok(())
    }

    /// Announce ourselves to the device's `remoted` as a modern (non-legacy)
    /// RemoteXPC peer.
    ///
    /// Send this only once per tunnel, after the device handshake has been received
    /// and parsed: the version flags and messaging protocol version are echoed from
    /// the device's handshake, and a failed parse means nothing was announced, so
    /// the handshake can be retried safely (同步 go-ios `6910f3b`/`51887da`：iOS 27.2
    /// 起每个隧道只接受一次 host announce，重复 announce 会使设备把所有服务迁到
    /// 新端口且旧端口立即失效；大 data 回复也只对已 announce 的 peer 可用).
    pub async fn send_host_handshake(
        &mut self,
        messaging_protocol_version: u64,
        version_flags: u64,
    ) -> Result<(), IdeviceError> {
        let msg = xpc!({
            "MessageType": "Handshake",
            "MessagingProtocolVersion": messaging_protocol_version,
            "UUID": uuid::Uuid::new_v4(),
            "Properties": {
                "BootSessionUUID": uuid::Uuid::new_v4(),
                "RemoteXPCVersionFlags": version_flags,
            },
            "Services": XPCObject::Dictionary(Dictionary::new())
        });

        self.send_object(msg, false).await
    }

    pub async fn recv(&mut self) -> Result<plist::Value, IdeviceError> {
        self.recv_from_channel(REPLY_CHANNEL).await
    }

    pub async fn recv_root(&mut self) -> Result<plist::Value, IdeviceError> {
        self.recv_from_channel(ROOT_CHANNEL).await
    }

    async fn recv_from_channel(&mut self, channel: u32) -> Result<plist::Value, IdeviceError> {
        loop {
            // Try to decode a whole message from what's already buffered before
            // reading more, so a message split across earlier reads completes.
            // Scope the buffer borrow so it's released before the await below.
            let decoded = {
                let buf = self.partial.entry(channel).or_default();
                match XPCMessage::decode(buf) {
                    Ok(msg) => {
                        // A complete wrapper consumes 24 + body_len bytes; drain
                        // exactly that, preserving any bytes of the next message.
                        let consumed = (XPC_WRAPPER_LEN + xpc_body_len(buf)).min(buf.len());
                        buf.drain(..consumed);
                        Some(msg)
                    }
                    // Not enough bytes yet: fall through to read another frame.
                    Err(IdeviceError::CdTunnel(CdTunnelError::SizeMismatch))
                    | Err(IdeviceError::NotEnoughBytes(..)) => None,
                    Err(e) => return Err(e),
                }
            };

            match decoded {
                Some(msg) => match msg.message {
                    // Skip empty-dictionary keepalives and bodyless frames.
                    Some(inner) => {
                        if let Some(d) = inner.as_dictionary()
                            && d.is_empty()
                        {
                            continue;
                        }
                        return Ok(inner.to_plist());
                    }
                    None => continue,
                },
                None => {
                    let chunk = self.h2_client.read(channel).await?;
                    let buf = self.partial.entry(channel).or_default();
                    // 部分消息缓冲是跨调用持久的：超过单消息上限说明对端损坏/恶意
                    // （body_len 声称远大于实际可送达的数据），清空缓冲并报错，防止
                    // 无界内存累积（rust-ios 受控补丁）。
                    if buf.len().saturating_add(chunk.len()) > MAX_XPC_MESSAGE_SIZE as usize {
                        buf.clear();
                        return Err(IdeviceError::UnexpectedResponse(format!(
                            "XPC partial message on channel {channel} exceeds the {} byte message limit",
                            MAX_XPC_MESSAGE_SIZE
                        )));
                    }
                    buf.extend(chunk);
                }
            }
        }
    }

    pub async fn send_object(
        &mut self,
        msg: impl Into<XPCObject>,
        expect_reply: bool,
    ) -> Result<(), IdeviceError> {
        let msg: XPCObject = msg.into();

        let mut flag = XPCFlag::DataFlag | XPCFlag::AlwaysSet;
        if expect_reply {
            flag |= XPCFlag::WantingReply;
        }

        let msg = XPCMessage::new(Some(flag), Some(msg), Some(self.root_id));
        self.send_root(msg).await?;

        Ok(())
    }

    async fn send_root(&mut self, msg: XPCMessage) -> Result<(), IdeviceError> {
        self.h2_client
            .send(msg.encode(self.root_id)?, ROOT_CHANNEL)
            .await?;
        Ok(())
    }

    async fn send_reply(&mut self, msg: XPCMessage) -> Result<(), IdeviceError> {
        self.h2_client
            .send(msg.encode(self.root_id)?, REPLY_CHANNEL)
            .await?;
        Ok(())
    }

    pub fn iter_file_chunks<'a>(
        &'a mut self,
        total_size: usize,
        file_idx: u32,
    ) -> impl Stream<Item = Result<Vec<u8>, IdeviceError>> + 'a {
        let stream_id = (file_idx + 1) * 2;

        try_stream! {
            fn strip_xpc_wrapper_prefix(buf: &[u8]) -> (&[u8], bool) {
                // Returns (data_after_wrapper, stripped_anything)
                const MAGIC: u32 = 0x29b00b92;

                if buf.len() < 24 {
                    return (buf, false);
                }

                let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                if magic != MAGIC {
                    return (buf, false);
                }

                // flags at [4..8] – not needed to compute size
                let body_len = u64::from_le_bytes([
                    buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
                ]) as usize;

                let wrapper_len = 24 + body_len;
                if buf.len() < wrapper_len {
                    // Incomplete wrapper (shouldn’t happen with your read API), keep as-is.
                    return (buf, false);
                }

                (&buf[wrapper_len..], true)
            }
            self.open_file_stream_for_response(stream_id).await?;

            let mut got = 0usize;
            while got < total_size {
                let bytes = self.h2_client.read(stream_id).await?;
                let (after, stripped) = strip_xpc_wrapper_prefix(&bytes);
                if stripped && after.is_empty() {
                    continue; // pure control wrapper, don't count
                }

                let data = if stripped { after.to_vec() } else { bytes };

                if data.is_empty() {
                    continue;
                }

                got += data.len();
                yield data;
            }
        }
    }

    pub async fn open_file_stream_for_response(
        &mut self,
        stream_id: u32,
    ) -> Result<(), IdeviceError> {
        // 1) Open the HTTP/2 stream
        self.h2_client.open_stream(stream_id).await?;

        // 2) Send an empty XPC wrapper on that same stream with FILE_TX_STREAM_RESPONSE
        let flags = XPCFlag::AlwaysSet | XPCFlag::FileTxStreamResponse;

        let msg = XPCMessage::new(Some(flags), None, Some(0));

        // IMPORTANT: send on `stream_id`, not ROOT/REPLY
        let bytes = msg.encode(0)?;
        self.h2_client.send(bytes, stream_id).await?;
        Ok(())
    }

    /// Open a stream for a reference-style file transfer payload（同步 go-ios
    /// `273d3e0` 的 `OpenFileTransfer`）。`XPCObject::FileTransfer` 在消息体中只携带
    /// 引用（msg id + TransferSize），载荷在本流的后续 DATA 帧上裸传；设备按引用中
    /// 的大小精确读取，因此无需半关闭。对端只有在收到引用该传输的消息后才接受此流，
    /// 所以发送载荷前必须先 [`RemoteXpcClient::wait_file_transfer_accepted`]。
    ///
    /// `stream_id` 由调用方分配（避开 root=1/reply=3；惯例 `2*(id+1)`）。
    pub async fn open_file_transfer(&mut self, id: u64, stream_id: u32) -> Result<(), IdeviceError> {
        self.h2_client.open_stream(stream_id).await?;
        // FileOpen 消息：`AlwaysSet | FileOpenFlag`（wire 值 0x00100000，即本 crate
        // 的 `FileTxStreamRequest`，与 go-ios `encoding.go` 的常量一致），无 body，
        // message id = 传输 id。
        let msg = XPCMessage::new(
            Some(XPCFlag::AlwaysSet | XPCFlag::FileTxStreamRequest),
            None,
            Some(id),
        );
        self.h2_client.send(msg.encode(id)?, stream_id).await?;
        Ok(())
    }

    /// Wait until the peer accepted the file transfer stream（go-ios
    /// `WaitAccepted`）。设备以 `FileOpenReplyFlag`（wire 值 0x00200000，即本 crate
    /// 的 `FileTxStreamResponse`）且 message id 等于传输 id 的空消息应答。
    pub async fn wait_file_transfer_accepted(
        &mut self,
        id: u64,
        stream_id: u32,
    ) -> Result<(), IdeviceError> {
        let bytes = self.h2_client.read(stream_id).await?;
        let msg = XPCMessage::decode(&bytes)?;
        let reply_id = msg.message_id.unwrap_or(0);
        if msg.flags & u32::from(XPCFlag::FileTxStreamResponse) == 0 || reply_id != id {
            return Err(IdeviceError::UnexpectedResponse(format!(
                "file transfer {id} not accepted: reply has flags 0x{:x} and id {reply_id}",
                msg.flags
            )));
        }
        Ok(())
    }

    /// Write a raw file-transfer payload chunk on the transfer stream（go-ios
    /// `FileTransferStream.Write`）。流控与帧分块由 HTTP/2 客户端处理。
    pub async fn write_file_transfer_chunk(
        &mut self,
        stream_id: u32,
        payload: Vec<u8>,
    ) -> Result<(), IdeviceError> {
        self.h2_client.send(payload, stream_id).await
    }
}

/// The XPC message body length from a decoded-OK wrapper: the little-endian u64
/// at bytes 8..16. `buf` is known to hold a full wrapper (≥ 24 bytes) here.
fn xpc_body_len(buf: &[u8]) -> usize {
    u64::from_le_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]) as usize
}

#[cfg(test)]
mod file_transfer_tests {
    use super::*;
    use format::Dictionary;
    use indexmap::indexmap;

    #[test]
    fn file_open_message_roundtrips_flags_and_id() {
        // go-ios FileOpen: Flags = AlwaysSet|FileOpenFlag(0x00100000), 无 body,
        // Id = 传输 id。
        let msg = XPCMessage::new(
            Some(XPCFlag::AlwaysSet | XPCFlag::FileTxStreamRequest),
            None,
            Some(7),
        );
        let bytes = msg.encode(7).unwrap();
        let decoded = XPCMessage::decode(&bytes).unwrap();
        assert_eq!(decoded.message_id, Some(7));
        assert_eq!(
            decoded.flags & u32::from(XPCFlag::FileTxStreamRequest),
            u32::from(XPCFlag::FileTxStreamRequest)
        );
        assert!(decoded.message.is_none());
    }

    #[test]
    fn file_open_reply_recognized_by_flag_and_id() {
        // go-ios FileOpenReply: Flags 含 FileOpenReplyFlag(0x00200000), Id 匹配。
        let reply = XPCMessage::new(
            Some(XPCFlag::AlwaysSet | XPCFlag::FileTxStreamResponse),
            None,
            Some(7),
        );
        let bytes = reply.encode(7).unwrap();
        let decoded = XPCMessage::decode(&bytes).unwrap();
        assert_eq!(decoded.message_id, Some(7));
        assert_ne!(
            decoded.flags & u32::from(XPCFlag::FileTxStreamResponse),
            0,
            "acceptance reply must carry the FileOpenReply flag"
        );
    }

    #[test]
    fn file_transfer_reference_roundtrips_transfer_size() {
        // go-ios encodeFileTransfer: type 0x0001a000 + msgId + {"s": TransferSize}。
        let reference = XPCObject::FileTransfer {
            msg_id: 2,
            data: Box::new(XPCObject::Dictionary(indexmap! {
                "s".to_string() => XPCObject::UInt64(512),
            })),
        };
        let bytes = reference.encode().unwrap();
        let decoded = XPCObject::decode(&bytes).unwrap();
        match decoded {
            XPCObject::FileTransfer { msg_id, data } => {
                assert_eq!(msg_id, 2);
                let dict: &Dictionary = data.as_dictionary().expect("dict payload");
                assert_eq!(dict.get("s"), Some(&XPCObject::UInt64(512)));
            }
            other => panic!("expected FileTransfer, got {other:?}"),
        }
    }
}
