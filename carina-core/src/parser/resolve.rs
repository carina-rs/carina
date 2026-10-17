//! Forward-reference resolution and identifier-scope checks.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::ast::{ParsedFile, ProviderConfig};
use super::entry::BindingSeed;
use super::error::{ParseError, undefined_identifier_error};
use super::static_eval::is_static_value;
use crate::eval_value::EvalValue;
use crate::resource::{
    ConcreteValue, DataSource, DeferredValue, InterpolationPart, Resource, Value,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use thiserror::Error;

/// One provider configuration whose construction-time attributes have crossed
/// an allowed resolution seam.
///
/// The inner parser value is private so provider construction cannot accept a
/// raw [`ProviderConfig`]. Instances come only from
/// [`resolve_provider_attributes_with_remote`], a checked
/// [`TryFrom`](std::convert::TryFrom) conversion of a deserialized
/// [`SavedProviderConfigs`], or the collection-owned implicit fallback returned
/// only when no block of the requested provider kind is declared.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProviderConfig {
    config: ProviderConfig,
}

impl ResolvedProviderConfig {
    fn from_resolved(config: ProviderConfig) -> Self {
        Self { config }
    }

    fn implicit(name: impl Into<String>) -> Self {
        Self {
            config: ProviderConfig {
                name: name.into(),
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn attributes(&self) -> &IndexMap<String, Value> {
        &self.config.attributes
    }

    pub fn source(&self) -> Option<&str> {
        self.config.source.as_deref()
    }

    pub fn binding(&self) -> Option<&str> {
        self.config.binding.as_deref()
    }

    pub fn is_default(&self) -> bool {
        self.config.is_default()
    }

    /// Borrow the serializable parser representation for metadata consumers.
    ///
    /// Provider-construction APIs intentionally do not accept this raw type.
    pub fn as_provider_config(&self) -> &ProviderConfig {
        &self.config
    }
}

/// Provider configurations whose construction-time attributes have crossed an
/// allowed resolution seam.
///
/// There is intentionally no `Default`, `From<Vec<ProviderConfig>>`, or
/// deserialization implementation. Raw parser configs cannot be used for
/// provider construction without first resolving upstream bindings or
/// validating a saved-plan payload.
///
/// ```compile_fail
/// use carina_core::parser::{ProviderConfig, ResolvedProviderConfigs};
///
/// let raw = Vec::<ProviderConfig>::new();
/// let _: ResolvedProviderConfigs = raw.into();
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProviderConfigs {
    configs: Vec<ResolvedProviderConfig>,
    provider_configs: Vec<ProviderConfig>,
}

impl ResolvedProviderConfigs {
    fn from_resolved(configs: Vec<ProviderConfig>) -> Result<Self, ProviderConfigInvariantError> {
        validate_resolved_provider_configs(&configs)?;
        let resolved = configs
            .iter()
            .cloned()
            .map(ResolvedProviderConfig::from_resolved)
            .collect();
        Ok(Self {
            configs: resolved,
            provider_configs: configs,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_test_configs(
        configs: Vec<ProviderConfig>,
    ) -> Result<Self, ProviderConfigInvariantError> {
        Self::from_resolved(configs)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResolvedProviderConfig> {
        self.configs.iter()
    }

    fn find_default(&self, name: &str) -> Option<&ResolvedProviderConfig> {
        self.configs
            .iter()
            .find(|config| config.is_default() && config.name() == name)
    }

    /// Select the configuration for an implicit operation of `name`.
    ///
    /// A default block wins. If the project declares only named instances,
    /// retain the historical first-declaration behavior. The empty fallback
    /// can be produced only here, and only when no block of this provider kind
    /// exists in this already-validated collection.
    pub fn provider_or_implicit(&self, name: &str) -> Cow<'_, ResolvedProviderConfig> {
        self.find_default(name)
            .or_else(|| self.configs.iter().find(|config| config.name() == name))
            .map_or_else(
                || Cow::Owned(ResolvedProviderConfig::implicit(name)),
                Cow::Borrowed,
            )
    }

    /// Borrow the resolved serializable shape used by saved plans and
    /// metadata-only consumers.
    pub fn as_provider_configs(&self) -> &[ProviderConfig] {
        &self.provider_configs
    }
}

/// Provider configurations crossing the saved-plan serialization seam.
///
/// The transparent representation preserves the historical JSON array shape.
/// Values can be created only from already-resolved configs or by
/// deserialization; converting them back to [`ResolvedProviderConfigs`]
/// revalidates every construction-time attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SavedProviderConfigs(Vec<ProviderConfig>);

#[derive(Clone, Copy)]
enum ProviderConfigSection {
    Attribute,
    DefaultTag,
}

const PROVIDER_CONFIG_SECTIONS: [ProviderConfigSection; 2] = [
    ProviderConfigSection::Attribute,
    ProviderConfigSection::DefaultTag,
];

fn provider_config_section(
    config: &ProviderConfig,
    section: ProviderConfigSection,
) -> &IndexMap<String, Value> {
    match section {
        ProviderConfigSection::Attribute => &config.attributes,
        ProviderConfigSection::DefaultTag => &config.default_tags,
    }
}

fn provider_config_section_mut(
    config: &mut ProviderConfig,
    section: ProviderConfigSection,
) -> &mut IndexMap<String, Value> {
    match section {
        ProviderConfigSection::Attribute => &mut config.attributes,
        ProviderConfigSection::DefaultTag => &mut config.default_tags,
    }
}

fn provider_config_secret_location(
    config: &ProviderConfig,
    section: ProviderConfigSection,
    key: &str,
) -> String {
    match section {
        ProviderConfigSection::Attribute => {
            format!("provider {}.{key}", config.name)
        }
        ProviderConfigSection::DefaultTag => {
            format!("provider {}.default_tags.{key}", config.name)
        }
    }
}

impl SavedProviderConfigs {
    /// Borrow the serialized provider configuration projection.
    pub fn as_provider_configs(&self) -> &[ProviderConfig] {
        &self.0
    }

    pub fn contains_redacted_secret(&self) -> bool {
        self.0.iter().any(|config| {
            let ProviderConfig {
                name: _name,
                attributes,
                default_tags,
                source: _source,
                version: _version,
                revision: _revision,
                unresolved_attributes,
                binding: _binding,
            } = config;
            attributes
                .values()
                .chain(default_tags.values())
                .chain(unresolved_attributes.values())
                .any(crate::value::contains_redacted_secret)
        })
    }

    /// Replace provider-configuration secrets with typed saved-plan
    /// placeholders while preserving the historical transparent array shape.
    pub fn redact_secrets(&self) -> Result<Self, crate::value::SerializationError> {
        let configs = self
            .0
            .iter()
            .map(|config| {
                let mut out = config.clone();
                for section in PROVIDER_CONFIG_SECTIONS {
                    let values = provider_config_section(config, section)
                        .iter()
                        .map(|(key, value)| {
                            crate::value::redact_secrets_in_value(value)
                                .map(|value| (key.clone(), value))
                        })
                        .collect::<Result<_, _>>()?;
                    *provider_config_section_mut(&mut out, section) = values;
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>, crate::value::SerializationError>>()?;
        Ok(Self(configs))
    }

    /// Restore provider-configuration placeholders from freshly evaluated
    /// provider configs and verify every plan-time hash.
    pub fn rehydrate_secrets(
        &self,
        evaluated: &Self,
    ) -> Result<Self, crate::value::SecretRehydrationSiteError> {
        let configs = self
            .0
            .iter()
            .map(|saved| {
                let has_placeholder = saved
                    .attributes
                    .values()
                    .chain(saved.default_tags.values())
                    .any(crate::value::contains_redacted_secret);
                if !has_placeholder {
                    return Ok(saved.clone());
                }
                let candidate = evaluated
                    .0
                    .iter()
                    .find(|candidate| {
                        candidate.name == saved.name && candidate.binding == saved.binding
                    })
                    .ok_or_else(|| crate::value::SecretRehydrationSiteError {
                        location: format!("provider {}", saved.name),
                        source: crate::value::SecretRehydrationError::ShapeMismatch,
                    })?;
                let mut out = saved.clone();
                for section in PROVIDER_CONFIG_SECTIONS {
                    for (key, saved_value) in provider_config_section(saved, section) {
                        let value = crate::value::rehydrate_secret_entry(
                            saved_value,
                            provider_config_section(candidate, section).get(key),
                            provider_config_secret_location(saved, section, key),
                            crate::value::SecretRehydrationCandidate::Evaluated,
                        )?;
                        if let Some(value) = value {
                            provider_config_section_mut(&mut out, section)
                                .insert(key.clone(), value);
                        }
                    }
                }
                Ok(out)
            })
            .collect::<Result<Vec<_>, crate::value::SecretRehydrationSiteError>>()?;
        Ok(Self(configs))
    }
}

impl From<&ResolvedProviderConfigs> for SavedProviderConfigs {
    fn from(configs: &ResolvedProviderConfigs) -> Self {
        Self(configs.provider_configs.clone())
    }
}

impl TryFrom<SavedProviderConfigs> for ResolvedProviderConfigs {
    type Error = ProviderConfigInvariantError;

    fn try_from(configs: SavedProviderConfigs) -> Result<Self, Self::Error> {
        Self::from_resolved(configs.0)
    }
}

#[cfg(test)]
mod saved_provider_config_secret_tests {
    use super::*;

    fn secret(value: &str) -> Value {
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String(value.into()),
        ))))
    }

    fn provider(token: &str) -> ProviderConfig {
        ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::from([("token".to_string(), secret(token))]),
            default_tags: IndexMap::from([("Token".to_string(), secret(token))]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: Some("secondary".to_string()),
        }
    }

    fn placeholder_hash(value: &Value) -> &crate::resource::RedactedSecretHash {
        match value {
            Value::Deferred(DeferredValue::RedactedSecret { hash }) => hash,
            other => panic!("expected saved-plan secret placeholder, got {other:?}"),
        }
    }

    #[test]
    fn provider_secret_placeholders_are_distinct_and_rehydrate() {
        let evaluated = SavedProviderConfigs(vec![provider("SAME_SECRET")]);
        let redacted = evaluated
            .redact_secrets()
            .expect("provider secrets should redact");
        let saved = &redacted.0[0];

        assert_ne!(
            placeholder_hash(saved.attributes.get("token").expect("token")),
            placeholder_hash(saved.default_tags.get("Token").expect("default tag")),
            "independent provider secrets must use independent random salts",
        );

        assert_eq!(
            redacted
                .rehydrate_secrets(&evaluated)
                .expect("unchanged provider secrets should rehydrate"),
            evaluated,
        );
    }

    #[test]
    fn provider_secret_rehydration_rejects_missing_matching_instance() {
        let planned = SavedProviderConfigs(vec![provider("SAME_SECRET")]);
        let redacted = planned.redact_secrets().expect("redact provider secret");
        let mut mismatched = provider("SAME_SECRET");
        mismatched.binding = Some("different-instance".to_string());

        let error = redacted
            .rehydrate_secrets(&SavedProviderConfigs(vec![mismatched]))
            .expect_err("a placeholder must map to its provider instance");
        assert!(matches!(
            error.source,
            crate::value::SecretRehydrationError::HashMismatch
                | crate::value::SecretRehydrationError::ShapeMismatch
        ));
    }
}

fn validate_resolved_provider_configs(
    configs: &[ProviderConfig],
) -> Result<(), ProviderConfigInvariantError> {
    for config in configs {
        for (attribute, value) in &config.attributes {
            crate::resource::assert_value_fully_resolved(value).map_err(|source| {
                ProviderConfigInvariantError::UnresolvedAttribute {
                    provider: config.name.clone(),
                    attribute: attribute.clone(),
                    source: Box::new(source),
                }
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProviderConfigInvariantError {
    #[error("provider {provider}: {attribute}: {source}")]
    UnresolvedAttribute {
        provider: String,
        attribute: String,
        #[source]
        source: Box<crate::value::SerializationError>,
    },
}

/// Failure while resolving a parsed provider configuration.
#[derive(Debug, Error)]
pub enum ProviderConfigResolutionError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Invariant(#[from] ProviderConfigInvariantError),
}

/// Mutable pre-apply traversal — the resolver mutates `attributes` and
/// `dependency_bindings` of every resource whose references are resolved
/// **before** apply: managed [`Resource`]s and [`DataSource`]s.
///
/// `Composition` is excluded on purpose — a composition resource's
/// attributes may carry refs whose resolution is deferred to the
/// post-apply path, so the pre-apply resolver must never rewrite them
/// (carina#3181; see the `composition.rs` module doc).
/// Pre-apply mutable accessors. Self-contained since #3308 retired
/// the `ResourceLike` trait — this trait keeps the read+write surface
/// the pre-apply resolver needs while staying scoped to the two
/// types that actually go through it ([`Resource`] and [`DataSource`]).
trait PreApplyResourceMut {
    fn attributes(&self) -> &IndexMap<String, Value>;
    fn dependency_bindings(&self) -> &BTreeSet<String>;
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value>;
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String>;
}

impl PreApplyResourceMut for Resource {
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependency_bindings
    }
}

impl PreApplyResourceMut for DataSource {
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependency_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependency_bindings
    }
}

/// Mutable iterator over the pre-apply resolvable resources — managed
/// resources then data sources (carina#3181). Used by the resolver to
/// rewrite refs in place across both typed slices in one walk.
fn iter_pre_apply_resources_mut(
    parsed: &mut ParsedFile,
) -> impl Iterator<Item = &mut dyn PreApplyResourceMut> + '_ {
    parsed
        .resources
        .iter_mut()
        .map(|r| r as &mut dyn PreApplyResourceMut)
        .chain(
            parsed
                .data_sources
                .iter_mut()
                .map(|d| d as &mut dyn PreApplyResourceMut),
        )
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    resolve_resource_refs_with_config(parsed, &ProviderContext::default())
}

/// Resolve resource references with the given parser configuration.
///
/// `parsed.user_functions` is consulted when resolving `Value::Deferred(DeferredValue::FunctionCall)`
/// values — a name that isn't a builtin but is a user-defined function in
/// the merged directory parse triggers user-fn evaluation here, even when
/// the per-file parse couldn't evaluate it because the `fn` declaration
/// lived in a sibling `.crn` (#2444).
pub fn resolve_resource_refs_with_config(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_policy(parsed, config, false, false)
}

/// Resolve one file before its sibling declarations have been merged.
/// Unknown function names are preserved for the directory-wide pass, where
/// the complete function table can distinguish a sibling function from a
/// genuinely unknown call.
pub(crate) fn resolve_resource_refs_defer_unknown_functions(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_policy(parsed, config, true, false)
}

/// Resolve the directory-wide semantic view used by diagnostics.
///
/// This keeps an authored user-function call when eager evaluation fails. The
/// boundary checker can then compare nested calls using the merged function
/// declarations instead of losing the call behind the evaluation error.
/// Truly unknown function names remain errors once this directory-wide pass
/// has the complete function table.
pub(crate) fn resolve_resource_refs_for_diagnostics(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_policy(parsed, config, false, true)
}

fn resolve_resource_refs_with_policy(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<(), ParseError> {
    // Save dependency bindings before resolution may change ResourceRef binding names.
    // This preserves direct dependencies that would be lost by recursive resolution
    // (e.g., tgw_attach.transit_gateway_id resolves to tgw.id, losing the tgw_attach dep).
    //
    // carina#3181: walk managed resources *and* data sources — a data
    // source's attributes can carry refs too, and `parsed.resources` is
    // now managed-only.
    for resource in iter_pre_apply_resources_mut(parsed) {
        // #3308: compute the value-ref dependency snapshot inline from
        // the trait's read accessors. The previous shared helper took
        // `&dyn ResourceLike`; with that trait gone, the equivalent is
        // a one-line attribute walk + dependency_bindings copy.
        let mut deps: std::collections::HashSet<String> = std::collections::HashSet::new();
        for value in resource.attributes().values() {
            crate::deps::collect_dependencies(value, &mut deps);
        }
        for name in resource.dependency_bindings() {
            deps.insert(name.clone());
        }
        if !deps.is_empty() {
            *resource.dependency_bindings_mut() = deps.into_iter().collect();
        }
    }

    // Build a map of binding_name -> attributes for quick lookup. Walk
    // every top-level resource (managed, data source, composition) so a
    // `ResourceRef` to any binding resolves. `binding_map` only needs
    // key-based lookup, not source order, so the inner map stays a plain
    // `HashMap`.
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
        }
    }

    // Register argument parameters so they're recognized as valid bindings
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }

    // Register module call bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved after module expansion.
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }

    // Register upstream_state bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved at plan time when the state file is loaded.
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.clone()).or_default();
    }

    // Register `wait` bindings the same way: downstream `<wait>.<attr>`
    // refs are passthrough of the target's snapshot, resolved at apply time.
    for wb in &parsed.wait_bindings {
        binding_map
            .entry(wb.binding.as_str().to_string())
            .or_default();
    }

    // Build a `ParseContext` once, populated with the merged
    // directory's user functions so a `Value::Deferred(DeferredValue::FunctionCall)` whose name
    // is a sibling-defined user-fn (visible only in the merged parse —
    // #2444) is evaluated here rather than rejected as "Unknown
    // built-in function". The HashMap is small (typically a handful of
    // user-defined functions). Restructuring the surrounding
    // `&mut parsed` borrow to avoid the clone is hairy on the error
    // path (would need to restore on every `?` exit), so we accept a
    // single map clone per call.
    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    // Backend getters consume ordinary values directly and have no secret
    // transport/writeback seam. Evaluate backend function calls with the same
    // semantics as resource attributes, then reject any secret-derived result
    // before validate/plan/apply can resolve a backend from it.
    if let Some(backend) = &mut parsed.backend {
        for (key, value) in &mut backend.attributes {
            let evaluated = resolve_function_calls_only(
                value,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            if crate::value::contains_secret(&evaluated) {
                let span = backend.attribute_spans.get(key).copied().ok_or_else(|| {
                    ParseError::InternalError {
                        expected: format!("source span for backend attribute '{key}'"),
                        context: "secret-derived backend attribute validation".to_string(),
                    }
                })?;
                return Err(ParseError::SecretDerivedBackendAttribute {
                    backend_type: backend.backend_type.clone(),
                    attribute: key.clone(),
                    line: span.start_line,
                    start_column: span.start_column,
                    end_column: span.end_column,
                });
            }
            *value = evaluated;
        }
    }

    // Resolve references in each resource. Keep `IndexMap` to preserve
    // the user's source order through resolution (#2222).
    //
    // carina#3181: managed resources and data sources both go through
    // pre-apply ref resolution; `Composition` is excluded — its refs
    // are resolved on the post-apply path.
    for resource in iter_pre_apply_resources_mut(parsed) {
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();

        for (key, expr) in resource.attributes().iter() {
            let resolved = resolve_value_with_config(
                expr,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            resolved_attrs.insert(key.clone(), resolved);
        }

        *resource.attributes_mut() = resolved_attrs;
    }

    // Residual `for` bodies are not part of `iter_pre_apply_resources_mut`:
    // their iterable is still unknown, but function calls in the template
    // can nevertheless have fully-known arguments. Evaluate those calls at
    // the same semantic seam as ordinary resources. In particular this turns
    // `secret(...)`, `decrypt(...)`, and expressions derived from them into
    // `DeferredValue::Secret` before saved-plan redaction; leaving the raw
    // call here would serialize its plaintext arguments from a deferred plan
    // effect. Keep resource references deferred so apply-time expansion still
    // substitutes the loop value and other runtime bindings normally.
    for deferred in &mut parsed.deferred_for_expressions {
        for value in deferred.template_resource.attributes.values_mut() {
            *value = resolve_function_calls_only(
                value,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
        }
        for (key, value) in &mut deferred.attributes {
            // `attributes` is the display projection of the same template.
            // Reuse the evaluated resource value so effect serialization is
            // consistent and side-effecting evaluators such as `decrypt()`
            // run only once.
            if let Some(evaluated) = deferred.template_resource.attributes.get(key) {
                *value = evaluated.clone();
            } else {
                *value = resolve_function_calls_only(
                    value,
                    &binding_map,
                    &fn_ctx,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                )?;
            }
        }
    }

    // Resolve top-level `let v = ...` bindings that contain
    // `Value::Deferred(DeferredValue::FunctionCall)` placeholders deferred by the per-file
    // parse — typically `fn X(...)` lives in a sibling `.crn` (#2444).
    // The variant below only mutates `FunctionCall` arms, leaving
    // other shapes (`Value::Concrete(ConcreteValue::String("${vpc}"))` placeholder,
    // `Value::Deferred(DeferredValue::ResourceRef)`, etc.) untouched so earlier passes
    // (`upstream_exports`, `BindingNameSet`) keep seeing the raw
    // unresolved forms they expect.
    let var_keys: Vec<String> = parsed.variables.keys().cloned().collect();
    for key in var_keys {
        let expr = parsed.variables[&key].clone();
        let resolved = resolve_function_calls_only(
            &expr,
            &binding_map,
            &fn_ctx,
            defer_unknown_functions,
            preserve_failed_user_calls,
        )?;
        parsed.variables[&key] = resolved;
    }

    // Resolve calls in every other value-bearing declaration as well. These
    // expressions may name a user function declared in a sibling `.crn`
    // file, so the per-file parser necessarily leaves a FunctionCall behind.
    // Type checking module arguments, attributes, or exports before this pass
    // would treat that deferred call as unknown and silently accept it.
    for call in &mut parsed.module_calls {
        for argument in call.arguments.values_mut() {
            *argument = resolve_function_calls_only(
                argument,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
        }
    }
    for argument in &mut parsed.arguments {
        if let Some(default) = &mut argument.default {
            *default = resolve_function_calls_only(
                default,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
        }
    }
    for attribute in &mut parsed.attribute_params {
        // Keep unannotated outputs in expression form until inference has
        // classified the function's return type. In particular, eagerly
        // reducing an `Any` builtin such as `lookup()` would incorrectly make
        // an otherwise-uninferrable output look like a concrete String.
        if attribute.type_expr.is_some()
            && let Some(value) = &mut attribute.value
        {
            *value = resolve_function_calls_only(
                value,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
        }
    }
    for export in &mut parsed.export_params {
        // See the matching attribute-output guard above. Annotated exports
        // need their concrete values for boundary checking; unannotated
        // exports must retain the expression for return-type inference.
        if export.type_expr.is_some()
            && let Some(value) = &mut export.value
        {
            *value = resolve_function_calls_only(
                value,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
        }
    }

    // Provider attribute resolution lives in
    // `resolve_provider_unresolved_attributes` (generic over export-
    // parameter shape so it works on both `ParsedFile` and `InferredFile`),
    // and is called by consumers after `module_resolver::resolve_modules_with_config`
    // produces composition resources. See #2717.

    Ok(())
}

/// Every binding name declared in the merged `ParsedFile`: resources,
/// arguments, module calls, upstream states, imports, user functions,
/// variables, and for/if structural bindings.
///
/// This is the canonical answer to "is this identifier in scope?" for
/// directory-wide checks. The same set feeds [`check_identifier_scope`]
/// and the LSP borrows it (via `carina_lsp::diagnostics::checks`) to
/// keep diagnostic suggestions consistent with the CLI.
///
/// Thin wrapper over [`crate::binding_index::BindingNameSet::from_parsed`]
/// (#2301). Prefer the new type at fresh call sites; this helper is kept
/// because `accumulate_*` helpers below still take `&HashSet<&str>` as a
/// borrowed view (changing that signature would force `undefined_identifier_error`
/// to rebuild a borrowed view per call).
pub fn collect_known_bindings_merged(parsed: &ParsedFile) -> std::collections::HashSet<&str> {
    let mut known = parsed.structural_binding_names();
    known.extend(parsed.variables.keys().map(String::as_str));
    known
}

/// Directory-wide parse seeds for Pass 2.
///
/// Plain value-shaped `let` bindings carry their pass-1 value so bare
/// sibling identifiers parse to that value instead of a deferred
/// placeholder. Structural bindings may also have parser placeholder
/// entries in `parsed.variables` (`"${binding}"`); those must not become
/// value seeds, so structural names continue to use the placeholder
/// installed by `seed_bindings`.
#[cfg(test)]
pub(crate) fn collect_seed_bindings_merged(parsed: &ParsedFile) -> Vec<BindingSeed<'_>> {
    let structural = parsed.structural_binding_names();

    collect_seed_bindings_from_parts(&structural, &parsed.variables)
}

pub(crate) fn collect_seed_bindings_from_parts<'a>(
    structural: &HashSet<&'a str>,
    variables: &'a IndexMap<String, Value>,
) -> Vec<BindingSeed<'a>> {
    let mut names: Vec<&'a str> = structural.iter().copied().collect();
    names.extend(variables.keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            if structural.contains(name) {
                BindingSeed::structural(name)
            } else if let Some(value) = variables.get(name) {
                BindingSeed::value(name, value)
            } else {
                unreachable!(
                    "collect_seed_bindings_from_parts names are either structural or in variables"
                )
            }
        })
        .collect()
}

pub(crate) fn reject_cyclic_let_bindings(parsed: &ParsedFile) -> Result<(), ParseError> {
    let structural = parsed.structural_binding_names();
    reject_cyclic_let_bindings_in_variables(&parsed.variables, &structural)
}

pub(crate) fn reject_cyclic_let_bindings_in_variables(
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
) -> Result<(), ParseError> {
    if let Some(chain) = find_let_binding_cycle(variables, structural) {
        return Err(ParseError::CyclicLetBinding { chain });
    }

    Ok(())
}

fn find_let_binding_cycle(
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
) -> Option<Vec<String>> {
    let mut visited = HashSet::new();
    let mut visiting = Vec::new();

    for name in variables.keys() {
        if structural.contains(name.as_str()) {
            continue;
        }
        if let Some(chain) =
            visit_let_binding(name, variables, structural, &mut visited, &mut visiting)
        {
            return Some(chain);
        }
    }

    None
}

fn visit_let_binding(
    name: &str,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    visited: &mut HashSet<String>,
    visiting: &mut Vec<String>,
) -> Option<Vec<String>> {
    if visited.contains(name) {
        return None;
    }
    if let Some(pos) = visiting.iter().position(|candidate| candidate == name) {
        let mut cycle = visiting[pos..].to_vec();
        cycle.push(name.to_string());
        return Some(cycle);
    }

    let value = variables.get(name)?;
    visiting.push(name.to_string());

    let roots = reference_roots_in_value(value, variables, structural);
    for root in roots {
        if let Some(chain) = visit_let_binding(&root, variables, structural, visited, visiting) {
            return Some(chain);
        }
    }

    visiting.pop();
    visited.insert(name.to_string());
    None
}

fn reference_roots_in_value(
    value: &Value,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
) -> Vec<String> {
    let mut roots = Vec::new();
    collect_reference_roots(value, variables, structural, &mut roots);
    roots
}

fn collect_reference_roots(
    value: &Value,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    roots: &mut Vec<String>,
) {
    match value {
        Value::Concrete(ConcreteValue::EnumIdentifier(name)) => {
            // Bare-identifier fallback (`EnumIdentifier("B")`) and dotted-access
            // fallback (`EnumIdentifier("B.attr")`) both appear before the parser
            // classifies the RHS as a binding ref or resource ref.
            push_root(name.as_str(), variables, structural, roots);
            if let Some((root, _)) = name.as_str().split_once('.') {
                push_root(root, variables, structural, roots);
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                collect_reference_roots(item, variables, structural, roots);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for item in map.values() {
                collect_reference_roots(item, variables, structural, roots);
            }
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            push_root(binding, variables, structural, roots);
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            push_root(path.binding(), variables, structural, roots);
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            for arg in args {
                collect_reference_roots(arg, variables, structural, roots);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(expr) = part {
                    collect_reference_roots(expr, variables, structural, roots);
                }
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            collect_reference_roots(inner, variables, structural, roots);
        }
        Value::Concrete(
            ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::String(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(DeferredValue::RedactedSecret { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => {}
    }
}

fn push_root(
    root: &str,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    roots: &mut Vec<String>,
) {
    if variables.contains_key(root)
        && !structural.contains(root)
        && !roots.iter().any(|existing| existing == root)
    {
        roots.push(root.to_string());
    }
}

/// Directory-wide identifier-scope validation for a merged [`ParsedFile`].
///
/// Emits one flat list of `UndefinedIdentifier` errors covering:
///
/// - Every `ResourceRef` whose root binding is not in scope (roots in
///   resource attributes, attribute-parameter values, module-call
///   arguments, and export-parameter values).
/// - Every deferred for-expression iterable whose root is not in scope.
///
/// Errors are returned in a deterministic order: ResourceRef findings
/// first (in resource / attribute / module / export order), then
/// deferred-iterable findings. The caller (CLI `load_configuration_with_config`,
/// LSP analysis pipeline) just inspects the returned `Vec` — both
/// checks share the same `collect_known_bindings_merged` pass, so there
/// is no performance reason to split them at the callsite.
///
/// **This is the canonical entry point for "is this identifier in
/// scope?" checks.** Follow the #2104 rule: any new semantic check in
/// that family gets added *here*, not as a new sibling function.
pub fn check_identifier_scope(parsed: &ParsedFile) -> Vec<ParseError> {
    let known = collect_known_bindings_merged(parsed);
    let mut errors = Vec::new();
    accumulate_undefined_reference_errors(parsed, &known, &mut errors);
    accumulate_deferred_iterable_errors(parsed, &known, &mut errors);
    errors
}

/// Validate every `directives { provider = <ident> }` reference and
/// every implicit default-instance lookup against `parsed.providers`.
///
/// Emits an error for each:
///
/// - `provider = <binding>` whose binding is not a declared
///   `ProviderInstance` (typo or undeclared name).
/// - `provider = <binding>` whose binding's kind does not match the
///   resource's kind (e.g. `aws.s3.Bucket { directives { provider =
///   <awscc-binding> } }`).
/// - Resource that omits `directives { provider = ... }` but whose
///   kind has no default instance (the kind was registered via a
///   top-level `provider <kind> { source = ..., version = ... }`
///   block with no instance attributes, and every instance of that
///   kind is named).
///
/// The check is a sibling of `check_identifier_scope` rather than
/// folded into it: the errors are provider-specific, the inputs
/// (`parsed.providers`) and outputs (kind-aware diagnostics) differ
/// from the generic "is this identifier in scope" walk, and folding
/// them together would force callers that only want one kind of
/// validation to pay for both.
pub fn check_provider_instance_routing(parsed: &ParsedFile) -> Vec<ParseError> {
    let mut errors = Vec::new();
    // carina#3181: walk the typed top-level slices. `directives()` is
    // `None` for the composition arm (no `directives` on a synthetic node),
    // which routes to the `None` match arm — and a composition's `id` has an
    // empty provider, so it is skipped there, matching prior behaviour.
    for rref in parsed.iter_top_level_resources() {
        let id = rref.id();
        let provider_instance = rref.directives().and_then(|d| d.provider_instance.as_ref());
        match provider_instance {
            Some(binding) => {
                let Some(instance) = parsed
                    .providers
                    .iter()
                    .find(|p| p.binding.as_deref() == Some(binding.as_str()))
                else {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "directives.provider: `{binding}` is not a known \
                             provider instance. Declare it with `let {binding} \
                             = provider <kind> {{ ... }}`."
                        ),
                    });
                    continue;
                };
                if instance.name != id.provider {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "directives.provider: instance `{binding}` has kind \
                             `{}`, but resource `{}.{}` requires kind `{}`",
                            instance.name, id.provider, id.resource_type, id.provider,
                        ),
                    });
                }
            }
            None => {
                // Mock / synthetic resources (no provider prefix) have no
                // kind to route by; skip them.
                if id.provider.is_empty() {
                    continue;
                }
                let (kind_has_any, kind_has_default) =
                    parsed
                        .providers
                        .iter()
                        .fold((false, false), |(any, def), p| {
                            let matches = p.name == id.provider;
                            (any || matches, def || (matches && p.is_default()))
                        });
                if !kind_has_any {
                    continue;
                }
                if !kind_has_default {
                    errors.push(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "resource `{}.{}` has no default provider \
                             instance for kind `{}`: either add instance \
                             attributes to the top-level `provider {}` \
                             block, or set `directives {{ provider = \
                             <instance> }}` explicitly.",
                            id.provider, id.resource_type, id.provider, id.provider,
                        ),
                    });
                }
            }
        }
    }
    errors
}

fn accumulate_undefined_reference_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut check = |value: &Value| {
        value.visit_resource_refs(&mut |path| {
            let root = path.binding();
            let root_ident = root.split(['[', ']']).next().unwrap_or(root);
            if !known.contains(root_ident) && seen.insert(root_ident.to_string()) {
                errors.push(undefined_identifier_error(known, root_ident.to_string(), 0));
            }
        });
    };

    // carina#3181: walk the typed top-level slices (managed, composition,
    // data source) — `parsed.resources` is managed-only now. Deferred
    // for-expression templates are handled by
    // `accumulate_deferred_iterable_errors`, so they are excluded here.
    for rref in parsed.iter_top_level_resources() {
        for value in rref.attributes().values() {
            check(value);
        }
    }
    for attr in &parsed.attribute_params {
        if let Some(value) = &attr.value {
            check(value);
        }
    }
    for call in &parsed.module_calls {
        for v in call.arguments.values() {
            check(v);
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = &export.value {
            check(value);
        }
    }
    for provider in &parsed.providers {
        for value in provider.attributes.values() {
            check(value);
        }
        for value in provider.unresolved_attributes.values() {
            check(value);
        }
    }
}

fn accumulate_deferred_iterable_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    for d in &parsed.deferred_for_expressions {
        if !known.contains(d.iterable_binding.as_str()) {
            errors.push(undefined_identifier_error(
                known,
                d.iterable_binding.clone(),
                d.line,
            ));
        }
    }
}

fn resolve_value_with_config(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    fn_ctx: &super::ParseContext<'_>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<Value, ParseError> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            match binding_map.get(path.binding()) {
                Some(attributes) => match attributes.get(path.attribute()) {
                    Some(attr_value) => {
                        // Recursively resolve in case the attribute itself is a reference
                        resolve_value_with_config(
                            attr_value,
                            binding_map,
                            fn_ctx,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )
                    }
                    None => {
                        // Attribute not found, keep as reference (might be resolved at runtime)
                        Ok(value.clone())
                    }
                },
                // Binding not found in *this* file's binding_map. The same
                // resolver runs both per-file (config_loader.rs L93) and on
                // the merged `ParsedFile` (L178); a per-file miss may be a
                // legitimate cross-file ref (`upstream_state` declared in a
                // sibling `.crn`, resource binding from another file).
                // Keep the ref as-is and let the post-merge
                // `check_identifier_scope` walk surface any genuine
                // undefined identifiers — that walk has full directory
                // context plus did-you-mean suggestions.
                None => Ok(value.clone()),
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|item| {
                    resolve_value_with_config(
                        item,
                        binding_map,
                        fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )
                })
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_value_with_config(
                        v,
                        binding_map,
                        fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?,
                );
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_value_with_config(
                            v,
                            binding_map,
                            fn_ctx,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved?)).canonicalize())
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let resolved_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| {
                    resolve_value_with_config(
                        a,
                        binding_map,
                        fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )
                })
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(is_static_value);

            let eval_args: Vec<EvalValue> = resolved_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, fn_ctx.config) {
                Ok(result) => result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                         finish the partial application before using the result as data",
                            name, leak.name, leak.remaining_arity
                        ),
                    }),
                Err(e) => {
                    // Builtin lookup failed. The name may be a user-fn
                    // that became visible only after directory merge
                    // (#2444): evaluate against the merged `fn_ctx`
                    // built once at the top of the resolver pass.
                    // Truly-undefined names with all-static args still
                    // error.
                    if let Some(user_fn) = fn_ctx.user_functions.get(name) {
                        let evaluated = super::functions::evaluate_user_function(
                            user_fn,
                            &resolved_args,
                            fn_ctx,
                        );
                        if preserve_failed_user_calls && evaluated.is_err() {
                            return Ok(value.clone());
                        }
                        return evaluated;
                    }
                    if all_args_resolved && !defer_unknown_functions {
                        // All args are resolved but builtin failed — propagate the error
                        Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("{}(): {}", name, e),
                        })
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::Deferred(DeferredValue::FunctionCall {
                            name: name.clone(),
                            args: resolved_args,
                        }))
                    }
                }
            }
        }
        _ => Ok(value.clone()),
    }
}

/// Walk a value tree and finalize any `Value::Deferred(DeferredValue::FunctionCall)` placeholder
/// that the per-file parse deferred (typically because the user-fn
/// lives in a sibling `.crn` — #2444). Recurses through `List` / `Map`
/// / `Interpolation` containers so deferred calls inside them surface
/// too. Non-FunctionCall arms (`String`, `ResourceRef`,
/// `Interpolation` literals, etc.) pass through unchanged so earlier
/// passes that read raw forms keep working.
fn resolve_function_calls_only(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    fn_ctx: &super::ParseContext<'_>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<Value, ParseError> {
    use crate::resource::InterpolationPart;
    match value {
        Value::Deferred(DeferredValue::FunctionCall { .. }) => resolve_value_with_config(
            value,
            binding_map,
            fn_ctx,
            defer_unknown_functions,
            preserve_failed_user_calls,
        ),
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|v| {
                    resolve_function_calls_only(
                        v,
                        binding_map,
                        fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )
                })
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(resolved?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_function_calls_only(
                        v,
                        binding_map,
                        fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?,
                );
            }
            Ok(Value::Concrete(ConcreteValue::Map(resolved)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_function_calls_only(
                            v,
                            binding_map,
                            fn_ctx,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved?)).canonicalize())
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Ok(Value::Deferred(
            DeferredValue::Secret(Box::new(resolve_function_calls_only(
                inner,
                binding_map,
                fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?)),
        )),
        _ => Ok(value.clone()),
    }
}

/// Resolve `ResourceRef` values inside provider blocks'
/// `unresolved_attributes` against the merged binding map produced by
/// `parsed.resources` + `parsed.arguments` + `parsed.module_calls` +
/// `parsed.upstream_states`.
///
/// Call this **after** `module_resolver::resolve_modules_with_config`
/// has produced composition resources for module-call bindings, so that
/// `default_tags = mod.tags` can see the module's exported attribute
/// values. Then call [`finalize_provider_configs`] to drain the
/// (now-literal) values into the typed `default_tags` field.
///
/// Generic over the export-parameter shape so callers can pass either
/// `ParsedFile` or `InferredFile` — we only touch
/// `provider.unresolved_attributes`, never `export_params`.
pub fn resolve_provider_unresolved_attributes<E>(
    parsed: &mut super::File<E>,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    // carina#3181: walk every top-level resource — managed, data source,
    // and composition. This runs after module expansion, so composition resources
    // exist and a provider attr like `default_tags = mod.tags` resolves
    // through the module-call composition.
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
        }
    }
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.clone()).or_default();
    }
    for wb in &parsed.wait_bindings {
        binding_map
            .entry(wb.binding.as_str().to_string())
            .or_default();
    }
    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    for provider in &mut parsed.providers {
        let mut resolved: IndexMap<String, Value> = IndexMap::new();
        for (key, expr) in &provider.unresolved_attributes {
            let r = resolve_value_with_config(expr, &binding_map, &fn_ctx, false, false)?;
            resolved.insert(key.clone(), r);
        }
        provider.unresolved_attributes = resolved;
    }
    Ok(())
}

/// Resolve `ResourceRef` values inside provider blocks' `attributes`
/// against a binding map built from `parsed`'s resources and `remote_bindings`
/// (the values fetched by the CLI's shared upstream-loading step).
///
/// This is the strict plan/apply-time counterpart to the edit-tolerant
/// [`resolve_provider_unresolved_attributes`] pass. It resolves both regular
/// provider attributes and the values inside `default_tags`; tag merging
/// remains deferred until desired normalization. The regular
/// `provider.attributes` map can contain refs nested inside
/// struct literals — `assume_role = { role_arn = upstream.arn }` is the
/// motivating case (carina#3182) — and those refs need to be substituted
/// before the attributes cross the WASM provider boundary in
/// `create_provider`.
///
/// Every construction-time provider attribute is resolved and then checked
/// before the typed result is minted. Unknown references therefore fail here;
/// validation/LSP paths that intentionally tolerate incomplete edits use the
/// lower-level parser resolution passes and never receive
/// [`ResolvedProviderConfigs`].
pub fn resolve_provider_attributes_with_remote<E>(
    parsed: &super::File<E>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
) -> Result<ResolvedProviderConfigs, ProviderConfigResolutionError> {
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
        }
    }
    for (binding, attrs) in remote_bindings {
        binding_map.insert(binding.clone(), attrs.clone());
    }

    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.user_functions = parsed.user_functions.clone();

    let mut providers = Vec::with_capacity(parsed.providers.len());
    for provider in &parsed.providers {
        let mut provider = provider.clone();
        for section in [&mut provider.attributes, &mut provider.default_tags] {
            let mut resolved = IndexMap::new();
            for (key, expr) in section.iter() {
                let value = resolve_value_with_config(expr, &binding_map, &fn_ctx, false, false)?;
                resolved.insert(key.clone(), value);
            }
            *section = resolved;
        }

        providers.push(provider);
    }
    ResolvedProviderConfigs::from_resolved(providers).map_err(Into::into)
}

/// Drain `ProviderConfig.unresolved_attributes` and promote resolved
/// well-known attributes into their typed fields. Run **after**
/// [`resolve_resource_refs`] (and, for module-call deferrals,
/// [`resolve_provider_unresolved_attributes`]) so deferred references
/// have a chance to resolve to literals first.
///
/// Today only `default_tags` is handled; `source` / `version` /
/// `revision` peel sites still use the legacy parse-time shape (#2757).
///
/// Errors when a resolved value has the wrong shape (e.g.
/// `default_tags = "string"`).
pub fn finalize_provider_configs<E>(parsed: &mut super::File<E>) -> Result<(), ParseError> {
    for provider in parsed.providers.iter_mut() {
        if let Some(value) = provider.unresolved_attributes.shift_remove("default_tags") {
            match value {
                Value::Concrete(ConcreteValue::Map(tags)) => {
                    provider.default_tags = tags;
                }
                other => {
                    return Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "Provider '{}': default_tags must resolve to a map, got {other:?}",
                            provider.name
                        ),
                    });
                }
            }
        }
        debug_assert!(
            provider.unresolved_attributes.is_empty(),
            "unresolved_attributes must be drained by finalize",
        );
    }
    Ok(())
}

#[cfg(test)]
mod seed_precedence_tests {
    use super::*;
    use crate::parser::entry::SeedKind;

    #[test]
    fn structural_let_with_placeholder_in_variables_seeds_as_structural() {
        // A `let cluster = mock.compute.Instance {}` shape: `cluster` appears
        // in `parsed.resources` and in `parsed.variables`, where let_binding.rs
        // stores the parser placeholder string "${cluster}".
        let mut parsed = ParsedFile::default();

        let mut resource = Resource::new("mock.compute.Instance", "instance");
        resource.binding = Some("cluster".to_string());
        parsed.resources.push(resource); // allow: direct — fixture test inspection

        parsed.variables.insert(
            "cluster".to_string(),
            Value::Concrete(ConcreteValue::String("${cluster}".into())),
        );

        let seeds = collect_seed_bindings_merged(&parsed);
        let cluster_seed = seeds
            .iter()
            .find(|seed| seed.name() == "cluster")
            .expect("cluster seed missing");

        match cluster_seed.kind() {
            SeedKind::Structural => {}
            SeedKind::Value(value) => panic!(
                "structural binding `cluster` with placeholder in variables \
                 must seed as Structural, not Value({value:?}). The structural-first \
                 precedence in collect_seed_bindings_merged is load-bearing for \
                 carina#3391's fix: if Value wins, the parser placeholder \
                 \"${{cluster}}\" leaks as the seed value and sibling-file `cluster.attr` \
                 references can resolve to the placeholder string instead of \
                 going through the resource-binding path."
            ),
        }
    }

    #[test]
    fn plain_let_literal_seeds_as_value() {
        // A `let bad_name = 'literal'` shape: `bad_name` appears in
        // `parsed.variables` only, not in any structural source.
        let mut parsed = ParsedFile::default();
        parsed.variables.insert(
            "bad_name".to_string(),
            Value::Concrete(ConcreteValue::String("literal".into())),
        );

        let seeds = collect_seed_bindings_merged(&parsed);
        let bad_seed = seeds
            .iter()
            .find(|seed| seed.name() == "bad_name")
            .expect("bad_name seed missing");

        match bad_seed.kind() {
            SeedKind::Value(value) => {
                assert_eq!(
                    value,
                    &Value::Concrete(ConcreteValue::String("literal".into())),
                    "Value seed must carry the actual literal"
                );
            }
            SeedKind::Structural => panic!("plain value let must seed as Value, not Structural"),
        }
    }

    #[test]
    fn deferred_for_templates_receive_function_evaluation() {
        let mut parsed = crate::parser::parse(
            r#"
let source = mock.compute.Instance {
  name = "source"
}

for item in source.arn {
  mock.test.resource {
    name    = "child"
    comment = secret("DEFERRED_TEMPLATE_PLAINTEXT")
  }
}
"#,
            &ProviderContext::default(),
        )
        .expect("parse deferred for fixture");
        let template = &parsed.deferred_for_expressions[0].template_resource;
        assert!(matches!(
            template.attributes.get("comment"),
            Some(Value::Deferred(DeferredValue::FunctionCall { name, .. })) if name == "secret"
        ));

        resolve_resource_refs_with_config(&mut parsed, &ProviderContext::default())
            .expect("resolve deferred template functions");

        let template = &parsed.deferred_for_expressions[0].template_resource;
        assert!(matches!(
            template.attributes.get("comment"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        assert!(matches!(
            parsed.deferred_for_expressions[0]
                .attributes
                .iter()
                .find_map(|(key, value)| (key == "comment").then_some(value)),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
    }
}
