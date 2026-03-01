use thiserror::Error;

/// Custom error type for the Soulseek download library
#[derive(Error, Debug)]
pub enum SoulseekRs {
    #[error("Network error: {0}")]
    NetworkError(#[from] std::io::Error),
    /// Authentication failed during login
    #[error("Authentication failed")]
    AuthenticationFailed,
    /// Error parsing messages or data
    #[error("Parse error: {0}")]
    ParseError(String),
    /// Operation timed out
    #[error("Operation timed out")]
    Timeout,
    /// Connection was closed unexpectedly
    #[error("Connection closed")]
    ConnectionClosed,
    /// Invalid message format or content
    #[error("Invalid message: {0}")]
    InvalidMessage(String),
    /// Server not connected
    #[error("Not connected to server")]
    NotConnected,
    /// Compression/decompression error
    #[error("Compression error: {0}")]
    CompressionError(String),
    /// Lock was poisoned
    #[error("Lock poisoned")]
    LockPoisoned,
}

impl From<std::num::ParseIntError> for SoulseekRs {
    fn from(err: std::num::ParseIntError) -> Self {
        SoulseekRs::ParseError(format!("Integer parse error: {}", err))
    }
}

impl From<String> for SoulseekRs {
    fn from(err: String) -> Self {
        SoulseekRs::CompressionError(err)
    }
}

/// Result type alias for the Soulseek library
pub type Result<T> = std::result::Result<T, SoulseekRs>;
