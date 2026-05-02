//! Shared helpers for LSP integration tests.
//!
//! Each integration test binary in this crate brings these in via
//! `mod support;` — the standard Cargo convention for cross-test code.

pub mod byte_helpers;
pub mod fixture;
pub mod test_client;

#[allow(unused_imports)]
pub use byte_helpers::find_subsequence;
#[allow(unused_imports)]
pub use test_client::TestClient;
