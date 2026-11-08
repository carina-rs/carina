//! Fixtures available to downstream tests with the `test-utils` feature.

#[cfg(not(target_arch = "wasm32"))]
pub mod loopback;

// Host boundary fixtures must not become guest display authority through
// Cargo feature unification in a provider's wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod provider_text;
