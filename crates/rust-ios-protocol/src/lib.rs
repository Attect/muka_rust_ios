//! rust-ios client/server wire protocol.
//!
//! This crate deliberately contains no device-specific code. Keeping the wire
//! contract independent lets the server implementation change without forcing
//! clients to understand Apple protocols.

mod codec;
pub mod junit;
mod model;

pub use codec::{FrameError, MAX_FRAME_LEN, read_json_frame, write_json_frame};
pub use model::*;

/// Major protocol versions must match exactly.
pub const PROTOCOL_MAJOR: u16 = 1;
/// Minor versions are backwards compatible within the same major version.
pub const PROTOCOL_MINOR: u16 = 20;
