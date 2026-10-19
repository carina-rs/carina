pub mod jsonrpc;
pub mod methods;
pub mod types;

/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 7;

/// Oldest provider protocol version this host can load safely.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 7;

/// Host-reserved object key for a typed redacted-secret digest.
pub const PERSISTED_SECRET_KEY: &str = "$secret";

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
