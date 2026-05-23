//! AWS KMS decryptor for the DSL `decrypt(...)` built-in.
//!
//! The parser's `ProviderContext.decryptor` is an
//! `Option<Box<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>>`.
//! `create_provider_context` in `carina-cli`'s `main` builds and
//! installs the production decryptor; this module owns the per-call
//! logic (base64 decode → `KMS:Decrypt` → response unwrap → UTF-8
//! decode) so a test can drive it with a mock `aws_sdk_kms::Client`
//! (see `carina-cli/tests/kms_decryptor_winterbaume.rs`, #3227).
//!
//! Two entry points share [`decrypt_one`]:
//!
//! - [`build_kms_decryptor`] (test seam): takes an already-built KMS
//!   client and returns a [`DecryptorFn`] that wraps it. Used by the
//!   integration test against `winterbaume`.
//! - `create_provider_context` in `main.rs` (production): keeps the
//!   `static OnceCell<aws_sdk_kms::Client>` it has always had so the
//!   real binary still loads SDK config lazily on the first
//!   `decrypt()` call. Its closure resolves the cell and then calls
//!   [`decrypt_one`] with the result.

use aws_sdk_kms::Client;
use aws_sdk_kms::primitives::Blob;
use base64::Engine;
use carina_core::parser::DecryptorFn;

/// Decrypt a single base64-encoded ciphertext using the given client.
///
/// All errors are prefixed with `decrypt():` so users see which DSL
/// built-in failed.
// Note: `pub` (not `pub(crate)`) because `carina-cli` exposes both a
// library (`lib.rs`) and a binary (`main.rs`), which cargo treats as
// distinct crates — the binary's `main.rs` imports this as an external
// crate (`carina_cli::kms::decrypt_one`), so `pub(crate)` would hide it.
pub async fn decrypt_one(
    client: &Client,
    ciphertext: &str,
    key: Option<&str>,
) -> Result<String, String> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(ciphertext)
        .map_err(|e| format!("decrypt(): invalid base64 ciphertext: {e}"))?;

    let mut req = client.decrypt().ciphertext_blob(Blob::new(blob));
    if let Some(k) = key {
        req = req.key_id(k);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("decrypt(): KMS decrypt failed: {e}"))?;

    let plaintext = resp
        .plaintext()
        .ok_or_else(|| "decrypt(): KMS response contained no plaintext".to_string())?;

    String::from_utf8(plaintext.as_ref().to_vec())
        .map_err(|e| format!("decrypt(): decrypted value is not valid UTF-8: {e}"))
}

/// Build a [`DecryptorFn`] backed by the given KMS client.
///
/// The returned closure uses `tokio::task::block_in_place` +
/// `Handle::current().block_on(...)` so it can be invoked
/// synchronously from inside parser evaluation.
///
/// # Panics
///
/// The returned closure panics if invoked outside a multi-threaded
/// tokio runtime: `tokio::task::block_in_place` panics on the
/// `current_thread` runtime and `Handle::current()` panics off any
/// runtime. Production calls go through `#[tokio::main]` (multi-thread
/// by default); tests must use `#[tokio::test(flavor = "multi_thread")]`.
pub fn build_kms_decryptor(client: Client) -> DecryptorFn {
    Box::new(move |ciphertext, key| {
        let ciphertext = ciphertext.to_string();
        let key = key.map(|k| k.to_string());
        let client = client.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { decrypt_one(&client, &ciphertext, key.as_deref()).await })
        })
    })
}
