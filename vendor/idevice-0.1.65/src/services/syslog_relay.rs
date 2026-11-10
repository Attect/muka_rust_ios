//! iOS Device SyslogRelay Service Abstraction

use crate::{Idevice, IdeviceError, IdeviceService, obf};

/// Client for interacting with the iOS device SyslogRelay service
#[derive(Debug)]
pub struct SyslogRelayClient {
    /// The underlying device connection with established SyslogRelay service
    pub idevice: Idevice,
    /// Persistent read buffer holding data read past the previous log's delimiter
    buffer: bytes::BytesMut,
}

impl IdeviceService for SyslogRelayClient {
    /// Returns the SyslogRelay service name as registered with lockdownd
    fn service_name() -> std::borrow::Cow<'static, str> {
        obf!("com.apple.syslog_relay")
    }

    async fn from_stream(idevice: Idevice) -> Result<Self, crate::IdeviceError> {
        Ok(Self::new(idevice))
    }
}

impl SyslogRelayClient {
    /// Creates a new SyslogRelay client from an existing device connection
    ///
    /// # Arguments
    /// * `idevice` - Pre-established device connection
    pub fn new(idevice: Idevice) -> Self {
        Self {
            idevice,
            buffer: bytes::BytesMut::with_capacity(1024),
        }
    }

    /// Get the next log from the relay
    ///
    /// # Returns
    /// A string containing the log, with syslog_relay's BSD vis(3) escapes decoded
    ///
    /// # Errors
    /// UnexpectedResponse if the service sends an EOF
    pub async fn next(&mut self) -> Result<String, IdeviceError> {
        let res = self
            .idevice
            .read_until_delim(&mut self.buffer, b"\n\x00")
            .await?;
        match res {
            Some(res) => Ok(decode_vis(&String::from_utf8_lossy(&res))),
            None => Err(IdeviceError::UnexpectedResponse(
                "syslog relay returned EOF".into(),
            )),
        }
    }
}

/// Reverses the BSD vis(3) encoding that syslog_relay applies to non-ASCII bytes
/// before sending log lines. Without decoding, multi-byte UTF-8 characters (e.g.
/// CJK) show up as escape sequences like `\M-f\M^H\M^V` instead of readable text.
///
/// Only the escapes syslog_relay emits for high bytes and backslashes are decoded:
///
/// - `\M-x` -> `0x80 | x` (meta)
/// - `\M^X` -> `0x80 | (X ^ 0x40)` (meta + control)
/// - `\ddd` -> octal byte value (vis octal-encodes `0xA0` as `\240` and, in
///   current Apple libc, backslash as `\134`)
/// - `\\` -> `\` (escaped backslash, older vis variants)
///
/// Octal escapes are only decoded to high bytes (>= 0x80) or printable ASCII;
/// control-valued octal escapes, control-character escapes (`\^X`) and anything
/// unrecognized are left untouched, so decoding never reintroduces raw control
/// bytes (e.g. ESC) into terminal output. Lone or truncated backslash sequences
/// pass through unchanged.
///
/// 同步 go-ios `a5bbcbe` + `1cc5c28`(`ios/syslog/vis.go`)。
pub fn decode_vis(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &bytes[i + 1..];
        if rest.len() >= 3 && rest[0] == b'M' && rest[1] == b'-' && is_graph(rest[2]) {
            out.push(rest[2] | 0x80);
            i += 4;
        } else if rest.len() >= 3 && rest[0] == b'M' && rest[1] == b'^' && is_meta_ctrl(rest[2]) {
            out.push((rest[2] ^ 0x40) | 0x80);
            i += 4;
        } else if rest.len() >= 3
            && is_octal(rest[0])
            && is_octal(rest[1])
            && is_octal(rest[2])
            && decodable_octal(octal_value(rest[0], rest[1], rest[2]))
        {
            out.push(octal_value(rest[0], rest[1], rest[2]) as u8);
            i += 4;
        } else if !rest.is_empty() && rest[0] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else {
            // lone backslash or unrecognized escape: keep as-is
            out.push(b'\\');
            i += 1;
        }
    }
    // 每个输出字节都来自输入字节或 0x80|graphic/0x80|control/受控八进制解码,
    // 控制值不会被重新引入,因此输出总是合法 UTF-8 或仅替换编码后的高字节序列。
    String::from_utf8_lossy(&out).to_string()
}

/// Reports whether c is a graphic ASCII character — the only payloads vis emits
/// after `\M-`.
fn is_graph(c: u8) -> bool {
    c > b' ' && c < 0x7f
}

/// Reports whether c is a payload vis emits after `\M^`: `@`..`_` for control
/// values 0x00-0x1F, or `?` for DEL.
fn is_meta_ctrl(c: u8) -> bool {
    (c >= b'@' && c <= b'_') || c == b'?'
}

fn is_octal(c: u8) -> bool {
    (b'0'..=b'7').contains(&c)
}

fn octal_value(a: u8, b: u8, c: u8) -> u32 {
    (u32::from(a - b'0') << 6) | (u32::from(b - b'0') << 3) | u32::from(c - b'0')
}

/// Reports whether an octal escape value is safe to decode: a high byte (part of
/// multi-byte UTF-8) or printable ASCII. Control values (and out-of-range ones
/// like `\777`) stay encoded.
fn decodable_octal(v: u32) -> bool {
    (0x80..=0xff).contains(&v) || (0x20..0x7f).contains(&v)
}

#[cfg(test)]
mod vis_tests {
    use super::decode_vis;

    #[test]
    fn plain_lines_pass_through() {
        assert_eq!(decode_vis("no escapes here"), "no escapes here");
        assert_eq!(decode_vis(""), "");
    }

    #[test]
    fn meta_escapes_decode_to_utf8_high_bytes() {
        // "\M-f\M^H\M^V" 是 UTF-8 "或" (0xE6 0x88 0x96) 的 vis 形态。
        assert_eq!(decode_vis("\\M-f\\M^H\\M^V"), "或");
        assert_eq!(decode_vis("value=\\M-f\\M^H\\M^V end"), "value=或 end");
    }

    #[test]
    fn octal_escapes_decode_high_bytes_and_printable_ascii() {
        // 完整 UTF-8 序列的八进制形态:"汉" (U+6C49 = 0xE6 0xB1 0x89) = \346\261\211。
        assert_eq!(decode_vis("\\346\\261\\211"), "汉");
        assert_eq!(decode_vis("value=\\346\\261\\211 end"), "value=汉 end");
        // 现行 Apple libc 把反斜杠编码为 \134。
        assert_eq!(decode_vis("back\\134slash"), "back\\slash");
        // 控制值八进制(\000 \033 \177)保持编码,终端安全策略。
        assert_eq!(decode_vis("esc=\\033."), "esc=\\033.");
        assert_eq!(decode_vis("nul=\\000."), "nul=\\000.");
        assert_eq!(decode_vis("del=\\177."), "del=\\177.");
        // 越界八进制(>0xFF)保持编码。
        assert_eq!(decode_vis("\\777"), "\\777");
        // 孤立高字节解码后不是合法 UTF-8,按 from_utf8_lossy 语义替换。
        assert_eq!(decode_vis("\\240"), "\u{fffd}");
    }

    #[test]
    fn control_and_unknown_escapes_stay_encoded() {
        // \^X 控制转义不在解码范围;孤立反斜杠与截断序列原样保留。
        assert_eq!(decode_vis("a\\^Cb"), "a\\^Cb");
        assert_eq!(decode_vis("trailing\\\\"), "trailing\\");
        assert_eq!(decode_vis("lone\\"), "lone\\");
        assert_eq!(decode_vis("\\M-"), "\\M-");
        // \M- 后跟非 graphic 字节(此处为 TAB)不构成 meta 转义,整体原样保留。
        assert_eq!(decode_vis("\\M-\t"), "\\M-\t");
    }
}

#[cfg(feature = "rsd")]
impl crate::RsdService for SyslogRelayClient {
    fn rsd_service_name() -> std::borrow::Cow<'static, str> {
        crate::obf!("com.apple.syslog_relay.shim.remote")
    }
    async fn from_stream(stream: Box<dyn crate::ReadWrite>) -> Result<Self, crate::IdeviceError> {
        let mut idevice = crate::Idevice::new(stream, "");
        idevice.rsd_checkin().await?;
        Ok(Self::new(idevice))
    }
}
