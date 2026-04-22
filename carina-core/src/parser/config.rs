//! Provider context for provider-injected validators and decryptor
//!
//! `ProviderContext` allows CLI/providers to inject custom type validators
//! and a decryptor function into the parser without using global mutable state.

use std::collections::{HashMap, HashSet};

/// Signature for a custom type validator function.
///
/// Takes a string value and returns `Ok(())` if valid, or `Err(message)` if invalid.
pub type ValidatorFn = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Signature for a factory-based custom type validator.
///
/// Takes `(type_name, value)` and returns `Ok(())` if valid or unknown,
/// or `Err(message)` if invalid.
pub type CustomTypeValidatorFn = Box<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

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
pub struct ProviderContext {
    /// Optional decryptor for the `decrypt()` built-in function.
    pub decryptor: Option<DecryptorFn>,
    /// Custom type validators keyed by type name (e.g., "arn", "availability_zone").
    pub validators: HashMap<String, ValidatorFn>,
    /// Factory-based custom type validator that calls through to provider factories
    /// (e.g., WASM plugins) for types not covered by `validators`.
    pub custom_type_validator: Option<CustomTypeValidatorFn>,
    /// Schema types registered by providers, keyed by
    /// `(provider, path, type_name)` (e.g., `("awscc", "ec2", "VpcId")`).
    ///
    /// Used by the parser to disambiguate 3+ segment paths: without this set,
    /// both `aws.ec2.Vpc` (resource kind) and `awscc.ec2.VpcId` (schema type)
    /// look identical. When a triple is present, the parser classifies the
    /// path as `TypeExpr::SchemaType`; otherwise it falls back to
    /// `TypeExpr::Ref`.
    pub schema_types: HashSet<(String, String, String)>,
}

impl ProviderContext {
    /// Register `(provider, path, type_name)` as a schema type so the parser
    /// classifies matching 3+ segment paths as `TypeExpr::SchemaType` rather
    /// than `TypeExpr::Ref`. Provider crates call this during setup.
    pub fn register_schema_type(
        &mut self,
        provider: impl Into<String>,
        path: impl Into<String>,
        type_name: impl Into<String>,
    ) {
        self.schema_types
            .insert((provider.into(), path.into(), type_name.into()));
    }

    /// Return `true` iff `(provider, path, type_name)` has been registered via
    /// [`register_schema_type`]. Used by the parser to route 3+ segment paths.
    pub fn is_schema_type(&self, provider: &str, path: &str, type_name: &str) -> bool {
        self.schema_types.contains(&(
            provider.to_string(),
            path.to_string(),
            type_name.to_string(),
        ))
    }
}

impl std::fmt::Debug for ProviderContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderContext")
            .field("decryptor", &self.decryptor.as_ref().map(|_| "..."))
            .field("validators", &self.validators.keys().collect::<Vec<_>>())
            .field(
                "custom_type_validator",
                &self.custom_type_validator.as_ref().map(|_| "..."),
            )
            .field("schema_types", &self.schema_types)
            .finish()
    }
}
