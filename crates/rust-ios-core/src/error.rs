use rust_ios_protocol::{ErrorCode, ProtocolError};
use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("usbmuxd unavailable: {0}")]
    UsbmuxdUnavailable(String),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("device unavailable: {0}")]
    DeviceUnavailable(String),
    #[error("pairing required: {0}")]
    PairingRequired(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("operation is not implemented: {0}")]
    NotImplemented(String),
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend error: {0}")]
    Backend(String),
}

impl CoreError {
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::UsbmuxdUnavailable(_)
                | Self::DeviceNotFound(_)
                | Self::DeviceUnavailable(_)
                | Self::Timeout(_)
                | Self::Io(_)
        )
    }
}

impl From<CoreError> for ProtocolError {
    fn from(error: CoreError) -> Self {
        let retryable = error.retryable();
        let code = match &error {
            CoreError::UsbmuxdUnavailable(_) => ErrorCode::UsbmuxdUnavailable,
            CoreError::DeviceNotFound(_) => ErrorCode::DeviceNotFound,
            CoreError::DeviceUnavailable(_) => ErrorCode::DeviceUnavailable,
            CoreError::PairingRequired(_) => ErrorCode::PairingRequired,
            CoreError::PermissionDenied(_) => ErrorCode::PermissionDenied,
            CoreError::NotImplemented(_) => ErrorCode::NotImplemented,
            CoreError::UnsupportedPlatform(_) => ErrorCode::UnsupportedPlatform,
            CoreError::InvalidArgument(_) => ErrorCode::InvalidRequest,
            CoreError::Timeout(_) => ErrorCode::Timeout,
            CoreError::Io(_) | CoreError::Backend(_) => ErrorCode::Internal,
        };
        ProtocolError::new(code, error.to_string()).retryable(retryable)
    }
}
