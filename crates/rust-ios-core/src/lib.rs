//! 设备协议与命令核心。
//!
//! 上层 server 只依赖本 crate 的稳定 trait，不直接依赖 `idevice`。这样普通测试可使用
//! [`MockDeviceBackend`]，真实设备实现的 API 变化也不会泄漏到 IPC。

#[cfg(feature = "real-device")]
mod accessibility;
#[cfg(feature = "real-device")]
mod activation;
mod app_store_connect;
mod backend;
#[cfg(feature = "real-device")]
mod cryptex;
#[cfg(feature = "real-device")]
mod ddi;
mod debug;
mod dproxy;
mod error;
mod file;
mod forward;
mod host;
mod management;
#[cfg(feature = "real-device")]
mod mcinstall;
mod mock;
#[cfg(feature = "real-device")]
mod real;
mod recording;
mod registry;
#[cfg(feature = "real-device")]
mod remote_pairing;
mod replay;
mod signing;
mod springboard;
#[cfg(feature = "real-device")]
mod supervised_pair;
#[cfg(feature = "real-device")]
mod supervision;
mod ui;
#[cfg(feature = "real-device")]
mod webinspector;

pub use backend::*;
pub use dproxy::*;
pub use error::*;
pub use forward::*;
pub use host::*;
pub use mock::*;
#[cfg(feature = "real-device")]
pub use real::*;
pub use recording::*;
pub use registry::*;
pub use replay::*;

fn mobilegestalt_envelope(diagnostics: plist::Dictionary) -> plist::Dictionary {
    plist::Dictionary::from_iter([
        (
            "Diagnostics".to_owned(),
            plist::Value::Dictionary(diagnostics),
        ),
        (
            "Status".to_owned(),
            plist::Value::String("Success".to_owned()),
        ),
    ])
}

fn plist_dictionary_xml(value: &plist::Dictionary) -> CoreResult<String> {
    let mut bytes = Vec::new();
    plist::to_writer_xml(&mut bytes, value)
        .map_err(|error| CoreError::Backend(format!("failed to serialize plist XML: {error}")))?;
    String::from_utf8(bytes)
        .map_err(|error| CoreError::Backend(format!("plist XML was not UTF-8: {error}")))
}
