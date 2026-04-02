pub mod jsonrpc;
pub mod methods;
pub mod types;

/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 1;

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
