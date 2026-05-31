//! Provider context for provider-injected validators and decryptor
//!
//! `ProviderContext` allows CLI/providers to inject custom type validators
//! and a decryptor function into the parser without using global mutable state.

use std::collections::{HashMap, HashSet};

use crate::schema::{SchemaRegistry, TypeIdentity};

/// Signature for a custom type validator function.
///
/// Takes a string value and returns `Ok(())` if valid, or `Err(message)` if invalid.
pub type ValidatorFn = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Signature for a factory-based custom type validator.
///
/// Takes `(identity, value)` and returns `Ok(())` if valid or unknown,
/// or `Err(message)` if invalid. The identity is structured so the
/// factory (e.g. a WASM provider) resolves the exact provider-scoped
/// type instead of splitting a flat name string.
pub type CustomTypeValidatorFn =
    Box<dyn Fn(&TypeIdentity, &str) -> Result<(), String> + Send + Sync>;

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
    /// Custom type validators keyed by structured [`TypeIdentity`], so
    /// two providers' same-named custom types resolve to distinct
    /// validators instead of colliding first-wins.
    pub validators: HashMap<TypeIdentity, ValidatorFn>,
    /// Factory-based custom type validator that calls through to provider factories
    /// (e.g., WASM plugins) for types not covered by `validators`.
    pub custom_type_validator: Option<CustomTypeValidatorFn>,
    /// Resource kinds loaded from provider schemas, keyed by `(provider,
    /// resource_type)` such as `("aws", "iam.Role")`.
    pub resource_types: HashSet<(String, String)>,
    /// Whether the provider-registration phase has populated this
    /// context with the full custom-type set. When `true`, the parser
    /// rejects any bare PascalCase type name that is not a built-in DSL
    /// custom type and is not present in [`validators`] as a bare
    /// identity — the carina#3239 root-cause fix that turns "silent
    /// accept of unknown custom types" into a parse error.
    ///
    /// The default is `false` so that early-parse paths that legitimately
    /// run before any provider has registered (LSP mid-edit reparse,
    /// `parse_type_expr_str` for completion ranking, unit tests built
    /// from string fixtures) keep their pre-#3239 behavior and do not
    /// lose every `Simple`-shaped type name to a hard error. CLI / LSP
    /// validation that *has* loaded schemas sets this to `true` so the
    /// strict check takes effect; see `enrich_provider_context` in the
    /// CLI command surface.
    pub customs_loaded: bool,
}

impl ProviderContext {
    /// Return `true` iff `(provider, resource_type)` is a loaded managed
    /// resource or data source kind.
    pub fn has_resource_type(&self, provider: &str, resource_type: &str) -> bool {
        self.resource_types
            .contains(&(provider.to_string(), resource_type.to_string()))
    }

    /// Build the resource-kind registry used to resolve dotted type refs.
    pub fn resource_types_from_schema_registry(
        schemas: &SchemaRegistry,
    ) -> HashSet<(String, String)> {
        schemas
            .iter()
            .map(|(provider, resource_type, _, _)| {
                (provider.to_string(), resource_type.to_string())
            })
            .collect()
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
            .field("resource_types", &self.resource_types)
            .field("customs_loaded", &self.customs_loaded)
            .finish()
    }
}
