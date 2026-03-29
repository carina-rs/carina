//! Parser configuration for provider-injected validators and decryptor
//!
//! `ParserConfig` allows CLI/providers to inject custom type validators
//! and a decryptor function into the parser without using global mutable state.

use std::collections::HashMap;

/// Signature for a custom type validator function.
///
/// Takes a string value and returns `Ok(())` if valid, or `Err(message)` if invalid.
pub type ValidatorFn = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Signature for a decryptor function.
///
/// Takes `(ciphertext, optional_key)` and returns the decrypted plaintext or an error.
pub type DecryptorFn = Box<dyn Fn(&str, Option<&str>) -> Result<String, String> + Send + Sync>;

/// Configuration for the parser, allowing providers to inject behavior.
///
/// This replaces the global `Mutex`-based decryptor registration and enables
/// provider-specific validators (e.g., ARN, availability_zone) to be injected
/// from provider crates while being callable during parsing.
#[derive(Default)]
pub struct ParserConfig {
    /// Optional decryptor for the `decrypt()` built-in function.
    pub decryptor: Option<DecryptorFn>,
    /// Custom type validators keyed by type name (e.g., "arn", "availability_zone").
    pub custom_validators: HashMap<String, ValidatorFn>,
}

impl std::fmt::Debug for ParserConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserConfig")
            .field("decryptor", &self.decryptor.as_ref().map(|_| "..."))
            .field(
                "custom_validators",
                &self.custom_validators.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}
