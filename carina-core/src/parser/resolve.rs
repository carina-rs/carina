//! Forward-reference resolution and identifier-scope checks.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::ast::{ExportParamLike, File, ParsedFile, ProviderConfig};
use super::entry::BindingSeed;
use super::error::{ParseError, undefined_identifier_error};
use super::static_eval::is_static_value;
use crate::eval_value::EvalValue;
use crate::resource::{
    AccessMissPolicy, ConcreteValue, DataSource, DeferredValue, DiagnosticSourceLine,
    InterpolationPart, Resource, UnavailableCoalescePolicy, UnavailableReason,
    UnavailableSecretPolicy, Value, find_unavailable_evaluation,
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

    pub fn source(&self) -> Option<&crate::provider_source::ProviderSource> {
        self.config.source.as_ref()
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
            crate::resource::assert_value_fully_resolved(value).map_err(|error| {
                ProviderConfigInvariantError::UnresolvedAttribute {
                    provider: config.name.clone(),
                    attribute: attribute.clone(),
                    error: Box::new(error),
                }
            })?;
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProviderConfigInvariantError {
    #[error("provider {provider}: {attribute}: {error}")]
    UnresolvedAttribute {
        provider: String,
        attribute: String,
        error: Box<crate::value::SerializationError>,
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
/// `LiveComposition` is excluded on purpose — a composition resource's
/// attributes may carry refs whose resolution is deferred to the
/// post-apply path, so the pre-apply resolver must never rewrite them
/// (carina#3181; see the `composition.rs` module doc).
/// Pre-apply mutable accessors. Self-contained since #3308 retired
/// the `ResourceLike` trait — this trait keeps the read+write surface
/// the pre-apply resolver needs while staying scoped to the two
/// types that actually go through it ([`Resource`] and [`DataSource`]).
trait PreApplyResourceMut {
    fn id(&self) -> &crate::resource::ResourceId;
    fn attributes(&self) -> &IndexMap<String, Value>;
    fn dependency_bindings(&self) -> &BTreeSet<String>;
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value>;
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String>;
}

impl PreApplyResourceMut for Resource {
    fn id(&self) -> &crate::resource::ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependencies.value_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependencies.value_bindings
    }
}

impl PreApplyResourceMut for DataSource {
    fn id(&self) -> &crate::resource::ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<String> {
        &self.dependencies.value_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<String> {
        &mut self.dependencies.value_bindings
    }
}

/// Mutable iterator over the pre-apply resolvable resources — managed
/// resources then data sources (carina#3181). Used by the resolver to
/// rewrite refs in place across both typed slices in one walk.
fn iter_pre_apply_resources_mut<E>(
    parsed: &mut File<E>,
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
pub fn resolve_resource_refs_with_config<E: ExportParamLike>(
    parsed: &mut File<E>,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_policy(parsed, config, false, false)
}

/// Evaluate deferred function calls in the raw resource snapshot retained for
/// identity and dependency provenance, while leaving ordinary references in
/// expression form. Sibling functions require the directory-merged function
/// table, so this runs after file merge rather than during each file's parse.
pub(crate) fn resolve_snapshot_function_calls_with_config(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in parsed.iter_top_level_resources() {
        if let Some(binding) = resource.binding() {
            binding_map.insert(binding.to_string(), resource.resolved_attributes());
        }
    }

    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.replace_user_functions(parsed.user_functions.clone());

    // This snapshot preserves authored references for identity/dependency
    // provenance; it is not a second validation surface. Provider-backed
    // annotations cannot be classified in the bootstrap context, and the
    // resolved tree runs their authoritative checks after schemas load.
    // Evaluate the same function bodies here without annotations so the raw
    // snapshot does not retain a user-function call that plan-time value
    // resolution would later mistake for an unknown builtin.
    for function in fn_ctx.user_functions_mut().values_mut() {
        for parameter in &mut function.params {
            parameter.param_type = None;
        }
        function.return_type = None;
    }
    for resource in iter_pre_apply_resources_mut(parsed) {
        for value in resource.attributes_mut().values_mut() {
            *value = resolve_function_calls_only(value, &binding_map, &fn_ctx, true, false)?;
        }
    }
    Ok(())
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

fn resolve_resource_refs_with_policy<E: ExportParamLike>(
    parsed: &mut File<E>,
    config: &ProviderContext,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<(), ParseError> {
    let pending_ctx = super::ParseContext::new(config);
    for check in parsed
        .parser_binding_metadata
        .pending_function_type_checks()
    {
        super::functions::validate_pending_function_type_check(
            check,
            &parsed.user_functions,
            &pending_ctx,
        )?;
    }

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
    fn_ctx.replace_user_functions(parsed.user_functions.clone());

    let resource_source_files: HashMap<crate::resource::ResourceId, std::path::PathBuf> = parsed
        .resources
        .iter()
        .map(|resource| &resource.id)
        .chain(parsed.data_sources.iter().map(|resource| &resource.id))
        .filter_map(|id| {
            parsed
                .parser_binding_metadata
                .resource_source_file(id)
                .map(|path| (id.clone(), path.to_path_buf()))
        })
        .collect();

    // The typed backend resolver consumes ordinary values directly and has no
    // secret transport/writeback seam. Evaluate backend function calls with
    // the same semantics as resource attributes, then reject any
    // secret-derived result before validate/plan/apply can resolve a backend.
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
                    line: span.start_line(),
                    start_column: span.start_column(),
                    end_column: span.end_column(),
                });
            }
            *value = evaluated;
        }
    }

    // Resolve references in each resource. Keep `IndexMap` to preserve
    // the user's source order through resolution (#2222).
    //
    // carina#3181: managed resources and data sources both go through
    // pre-apply ref resolution; `LiveComposition` is excluded — its refs
    // are resolved on the post-apply path.
    for resource in iter_pre_apply_resources_mut(parsed) {
        let source_file = resource_source_files.get(resource.id()).cloned();
        let mut resolved_attrs: IndexMap<String, Value> = IndexMap::new();

        for (key, expr) in resource.attributes().iter() {
            let resolved = resolve_value_with_config(
                expr,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| match &source_file {
                Some(path) => error.in_file(path.clone()),
                None => error,
            })?;
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
    for deferred in parsed.parsed_deferred_for_expressions_mut() {
        match &mut deferred.body {
            super::DeferredForBody::Resource(resource) => {
                for value in resource.attributes.values_mut() {
                    *value = resolve_function_calls_only(
                        value,
                        &binding_map,
                        &fn_ctx,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?;
                }
            }
            // Unsupported read and module-call bodies are retained verbatim
            // so the module-resolution seam can report their typed error.
            // They never become a resource template or a saved plan effect.
            super::DeferredForBody::Read(_) | super::DeferredForBody::ModuleCall(_) => {}
        }
    }

    // Resolve top-level `let v = ...` bindings that contain
    // `Value::Deferred(DeferredValue::FunctionCall)` placeholders deferred by the per-file
    // parse — typically `fn X(...)` lives in a sibling `.crn` (#2444).
    // The variant below only mutates `FunctionCall` arms, leaving
    // other shapes (`Value::Deferred(DeferredValue::BindingRef)` references,
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
        for argument in call.arguments_mut() {
            argument.try_rewrite(|value| {
                resolve_function_calls_only(
                    value,
                    &binding_map,
                    &fn_ctx,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                )
            })?;
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
        if attribute.static_type.is_some()
            && let Some(crate::parser::BoundaryExpression::Value(value)) = &mut attribute.value
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
        let has_static_type = export.type_expr_opt().is_some();
        if has_static_type && let Some(value) = export.value() {
            let resolved = resolve_function_calls_only(
                value,
                &binding_map,
                &fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            export.replace_value(resolved);
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
pub fn collect_known_bindings_merged<E>(parsed: &File<E>) -> std::collections::HashSet<&str> {
    let mut known = parsed.structural_binding_names();
    known.extend(parsed.variables.keys().map(String::as_str));
    known
}

/// Directory-wide parse seeds for Pass 2.
///
/// Plain value-shaped `let` bindings carry their pass-1 value so bare
/// sibling identifiers parse to that value instead of a deferred
/// placeholder. Structural names always become typed structural seeds and
/// remain outside `parsed.variables`.
#[cfg(test)]
pub(crate) fn collect_seed_bindings_merged(parsed: &ParsedFile) -> Vec<BindingSeed<'_>> {
    let structural = parsed.structural_binding_names();
    let module_parameters: HashMap<&str, &super::TypeExpr> = parsed
        .arguments
        .iter()
        .map(|argument| (argument.name.as_str(), &argument.static_type))
        .collect();

    collect_seed_bindings_from_parts(
        &structural,
        &module_parameters,
        &parsed.variables,
        &parsed.parser_binding_metadata,
    )
}

pub(crate) fn collect_seed_bindings_from_parts<'a>(
    structural: &HashSet<&'a str>,
    module_parameters: &HashMap<&'a str, &'a super::TypeExpr>,
    variables: &'a IndexMap<String, Value>,
    metadata: &'a super::ast::ParserBindingMetadata,
) -> Vec<BindingSeed<'a>> {
    let mut names: Vec<&'a str> = structural.iter().copied().collect();
    names.extend(variables.keys().map(String::as_str));
    names.extend(metadata.structural_handles().keys().map(String::as_str));
    names.sort_unstable();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            if let Some(declared_type) = module_parameters.get(name) {
                match metadata.structural_handles().get(name) {
                    Some(handle) => BindingSeed::typed_structural(name, handle),
                    None => BindingSeed::module_parameter(name, declared_type),
                }
            } else if let Some(handle) = metadata.structural_aliases().get(name) {
                BindingSeed::structural_alias(name, handle)
            } else if let Some(handle) = metadata.structural_handles().get(name) {
                BindingSeed::typed_structural(name, handle)
            } else if let Some(value) = variables.get(name) {
                // The bootstrap structural-name set can conservatively
                // contain an `if` binding whose provisional condition made
                // us inspect a structural branch. Once the fixed point has a
                // real value for that binding, the classified value wins.
                match metadata.value_structural_handles(name) {
                    Some(structural_handles) => {
                        BindingSeed::value_with_structural_handles(name, value, structural_handles)
                    }
                    None => BindingSeed::value(name, value),
                }
            } else if structural.contains(name) {
                // Function names participate in directory scope checks but
                // are not value-position structural handles.
                BindingSeed::provisional(name)
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
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            for nested in deferred.values() {
                collect_reference_roots(nested, variables, structural, roots);
            }
        }
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
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for arg in args {
                collect_reference_roots(arg.walker_value(), variables, structural, roots);
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            collect_reference_roots(access.base(), variables, structural, roots);
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
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            collect_reference_roots(value, variables, structural, roots);
            collect_reference_roots(default, variables, structural, roots);
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            collect_reference_roots(expression.lhs(), variables, structural, roots);
            collect_reference_roots(expression.rhs(), variables, structural, roots);
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
/// - Every deferred for-expression reference root whose iterable is not in
///   scope, including bare identifiers represented by the parser's distinct
///   undefined-iterable state.
///
/// **This is the canonical entry point for "is this identifier in
/// scope?" checks.** Follow the #2104 rule: any new semantic check in
/// that family gets added *here*, not as a new sibling function.
pub fn check_identifier_scope<E: ExportParamLike>(parsed: &File<E>) -> Vec<ParseError> {
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
pub fn check_provider_instance_routing<E>(parsed: &File<E>) -> Vec<ParseError> {
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
    parsed: &File<impl ExportParamLike>,
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
    // for-expression templates are handled separately by
    // `accumulate_deferred_iterable_errors`, so they are excluded here.
    for rref in parsed.iter_top_level_resources() {
        for value in rref.attributes().values() {
            check(value);
        }
    }
    for attr in &parsed.attribute_params {
        if let Some(crate::parser::BoundaryExpression::Value(value)) = &attr.value {
            check(value);
        }
    }
    for call in &parsed.module_calls {
        for (_, v) in call.arguments() {
            check(v.walker_value());
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value() {
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
    parsed: &File<impl ExportParamLike>,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    let mut known_bindings = known
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    known_bindings.sort_unstable();
    for deferred in parsed.parsed_deferred_for_expressions() {
        if let Some(error) = deferred.undefined_iterable_error(&known_bindings) {
            errors.push(error);
            continue;
        }
        for root in deferred.root_bindings() {
            if known.contains(root.as_str()) {
                continue;
            }
            errors.push(undefined_identifier_error(known, root, deferred.line));
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
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let deferred = deferred.try_map(|value| {
                resolve_value_with_config(
                    value,
                    binding_map,
                    fn_ctx,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                )
            })?;
            Ok(Value::Deferred(DeferredValue::ModuleIf(Box::new(deferred))))
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let left = resolve_value_with_config(
                left,
                binding_map,
                fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            if matches!(left, Value::Concrete(_)) {
                Ok(left)
            } else {
                // Carry the fallback through the same directory fixed point as
                // the left operand, while preserving failures as deferred calls
                // until absence actually selects it.
                let default = resolve_value_with_config(default, binding_map, fn_ctx, true, true)?;
                Ok(Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(left),
                    default: Box::new(default),
                }))
            }
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = resolve_value_with_config(
                expression.lhs(),
                binding_map,
                fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            let rhs = resolve_value_with_config(
                expression.rhs(),
                binding_map,
                fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            let span = expression.operator_span();
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, 0, error))
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            // A suffix is part of the authored reference, not decoration on
            // its base attribute. Resolving only `binding.attribute` would
            // erase the suffix (`q.name[0]` became the whole `q.name`
            // string), preventing static validation and changing runtime
            // semantics. Segmented references stay canonical ResourceRefs;
            // BindingIndex checks their shape and the apply resolver performs
            // the value navigation with the binding-source miss policy.
            if !path.segments().is_empty() {
                return Ok(value.clone());
            }
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
            let resolved = resolved?;
            reject_required_interpolation_unavailable(&resolved, defer_unknown_functions)?;
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved)).canonicalize())
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            reason,
            source_line,
            ..
        }) => {
            if defer_unknown_functions {
                Ok(value.clone())
            } else {
                Err(unavailable_builtin_error(*name, reason, *source_line))
            }
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let evaluation_line = source_line.get().unwrap_or(function.body.source_line);
            let resolved_args = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        resolve_value_with_config(
                            value,
                            binding_map,
                            fn_ctx,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let evaluated = super::functions::evaluate_resolved_user_function(
                function,
                functions,
                &resolved_args,
                fn_ctx.config,
                evaluation_line,
            );
            if preserve_failed_user_calls && evaluated.is_err() {
                return Ok(value.clone());
            }
            evaluated
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let authored_line = source_line.get();
            let resolved_args: Result<Vec<crate::resource::FunctionArgument>, ParseError> = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        resolve_value_with_config(
                            value,
                            binding_map,
                            fn_ctx,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )
                    })
                })
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(|argument| {
                argument.handle().is_some() || argument.value().is_some_and(is_static_value)
            });

            let eval_args: Vec<EvalValue> = resolved_args
                .iter()
                .map(super::functions::function_argument_as_eval)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(
                name.as_str(),
                &eval_args,
                fn_ctx.config,
            ) {
                Ok(result) => result.into_value().map_err(|error| match authored_line {
                    Some(line) => ParseError::from_eval_value(error, line),
                    None => ParseError::UserFunctionError(error.to_string()),
                }),
                Err(e) => {
                    if all_args_resolved {
                        let reason = UnavailableReason::from_builtin_error(e);
                        if defer_unknown_functions {
                            Ok(Value::Deferred(DeferredValue::Unavailable {
                                name: *name,
                                args: resolved_args,
                                reason,
                                source_line: *source_line,
                            }))
                        } else {
                            Err(unavailable_builtin_error(*name, &reason, *source_line))
                        }
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::Deferred(DeferredValue::FunctionCall {
                            name: *name,
                            args: resolved_args,
                            source_line: *source_line,
                        }))
                    }
                }
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let resolved_base = resolve_value_with_config(
                access.base(),
                binding_map,
                fn_ctx,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?;
            access
                .rebuild_with_base(resolved_base, AccessMissPolicy::Error)
                .map_err(|error| ParseError::InvalidExpression {
                    line: access.source_line(),
                    message: error.to_string(),
                })
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::BindingRef { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => Ok(value.clone()),
    }
}

fn unavailable_builtin_error(
    name: crate::builtins::BuiltinFunction,
    reason: &UnavailableReason,
    source_line: DiagnosticSourceLine,
) -> ParseError {
    let message = format!("{name}(): {reason}");
    match source_line.get() {
        Some(line) => ParseError::InvalidExpression { line, message },
        None => ParseError::UserFunctionError(message),
    }
}

fn reject_required_interpolation_unavailable(
    parts: &[InterpolationPart],
    defer_unknown_functions: bool,
) -> Result<(), ParseError> {
    if defer_unknown_functions {
        return Ok(());
    }
    if let Some((function, reason, source_line)) = parts.iter().find_map(|part| {
        if let InterpolationPart::Expr(value) = part {
            find_unavailable_evaluation(
                value,
                UnavailableCoalescePolicy::ExcludeDefault,
                UnavailableSecretPolicy::Exclude,
            )
        } else {
            None
        }
    }) {
        return Err(unavailable_builtin_error(function, reason, source_line));
    }
    Ok(())
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
        Value::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Coalesce { .. }
            | DeferredValue::IntegerArithmetic(_),
        ) => resolve_value_with_config(
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
            let resolved = resolved?;
            reject_required_interpolation_unavailable(&resolved, defer_unknown_functions)?;
            Ok(Value::Deferred(DeferredValue::Interpolation(resolved)).canonicalize())
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => Ok(value.clone()),
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
pub fn resolve_provider_unresolved_attributes<E, D>(
    parsed: &mut super::File<E, D>,
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
    fn_ctx.replace_user_functions(parsed.user_functions.clone());

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
pub fn resolve_provider_attributes_with_remote<E, D>(
    parsed: &super::File<E, D>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
) -> Result<ResolvedProviderConfigs, ProviderConfigResolutionError> {
    let mut binding_map = HashMap::new();
    let mut bindings = crate::binding_index::ResolvedBindings::default();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            binding_map.insert(binding_name.to_string(), rref.resolved_attributes());
            bindings.set_with_completeness(
                binding_name,
                rref.resolved_attributes(),
                crate::binding_index::BindingValueSource::Local,
                crate::binding_index::BindingCompleteness::Incomplete,
            );
        }
    }
    for (binding, attrs) in remote_bindings {
        binding_map.insert(binding.clone(), attrs.clone());
        bindings.set_with_completeness(
            binding,
            attrs.clone(),
            crate::binding_index::BindingValueSource::Upstream,
            crate::binding_index::BindingCompleteness::Complete,
        );
    }
    for upstream in &parsed.upstream_states {
        binding_map.entry(upstream.binding.clone()).or_default();
    }
    let mut fn_ctx = super::ParseContext::new(config);
    fn_ctx.replace_user_functions(parsed.user_functions.clone());

    let mut providers = Vec::with_capacity(parsed.providers.len());
    for provider in &parsed.providers {
        let mut provider = provider.clone();
        for section in [&mut provider.attributes, &mut provider.default_tags] {
            let mut resolved = IndexMap::new();
            for (key, expr) in section.iter() {
                let parser_resolved =
                    resolve_value_with_config(expr, &binding_map, &fn_ctx, false, false)?;
                let value = crate::resolver::resolve_ref_value_with_config(
                    &parser_resolved,
                    &bindings,
                    config,
                )
                .map_err(|message| ParseError::InvalidExpression { line: 0, message })?;
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
pub fn finalize_provider_configs<E, D>(parsed: &mut super::File<E, D>) -> Result<(), ParseError> {
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
mod resolve_access_tests {
    use super::*;

    #[test]
    fn resource_ref_resolution_never_discards_authored_path_segments() {
        let mut parsed = crate::parser::parse(
            r#"
let q = mock.test.resource { name = "q-z" }
let consumer = mock.test.resource { name = q.name[0] }
"#,
            &ProviderContext::default(),
        )
        .expect("parse resource path fixture");
        let authored = parsed.resources[1]
            .get_attr("name")
            .expect("consumer name")
            .clone();
        assert!(matches!(
            &authored,
            Value::Deferred(DeferredValue::ResourceRef { path })
                if path.segments().len() == 1
        ));

        resolve_resource_refs_with_config(&mut parsed, &ProviderContext::default())
            .expect("resolution keeps validation-owned paths deferred");

        assert_eq!(
            parsed.resources[1].get_attr("name"),
            Some(&authored),
            "resolving the known base attribute must not erase its postfix path",
        );
    }
}

#[cfg(test)]
mod unavailable_evaluation_tests {
    use super::*;

    const MISSING_ENV: &str = "CARINA_TEST_UNAVAILABLE_EVALUATION_3974";

    fn parsed_with_comment(expression: &str) -> ParsedFile {
        crate::parser::parse(
            &format!(
                r#"
let item = mock.test.resource {{
  name    = "unavailable-evaluation"
  comment = {expression}
}}
"#,
            ),
            &ProviderContext::default(),
        )
        .expect("parse unavailable evaluation fixture")
    }

    fn comment(parsed: &ParsedFile) -> &Value {
        parsed.resources[0]
            .get_attr("comment")
            .expect("fixture comment")
    }

    #[test]
    fn permissive_resolution_distinguishes_failed_and_pending_builtin_calls() {
        unsafe {
            std::env::remove_var(MISSING_ENV);
        }
        let mut failed = parsed_with_comment(&format!(r#"env("{MISSING_ENV}")"#));
        resolve_resource_refs_defer_unknown_functions(&mut failed, &ProviderContext::default())
            .expect("permissive resolution retains a diagnostic value");

        match comment(&failed) {
            Value::Deferred(DeferredValue::Unavailable {
                name, args, reason, ..
            }) => {
                assert_eq!(*name, crate::builtins::BuiltinFunction::env);
                assert!(matches!(
                    args.as_slice(),
                    [argument]
                        if matches!(
                            argument.value(),
                            Some(Value::Concrete(ConcreteValue::String(value)))
                                if value == MISSING_ENV
                        )
                ));
                assert_eq!(
                    reason.as_str(),
                    format!("environment variable '{MISSING_ENV}' is not set")
                );
            }
            other => panic!("failed ready call must be Unavailable, got {other:?}"),
        }
        assert!(!comment(&failed).is_fully_resolved());
        assert!(!is_static_value(comment(&failed)));

        let mut pending = crate::parser::parse(
            r#"
let origin = mock.test.resource { name = "origin" }
let item = mock.test.resource {
  name    = "pending-evaluation"
  comment = upper(origin.identifier)
}
"#,
            &ProviderContext::default(),
        )
        .expect("parse pending evaluation fixture");
        resolve_resource_refs_defer_unknown_functions(&mut pending, &ProviderContext::default())
            .expect("pending call remains deferred");
        assert!(matches!(
            pending.resources[1].get_attr("comment"),
            Some(Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                ..
            }))
        ));
    }

    #[test]
    fn strict_resolution_reports_the_original_unavailable_builtin_error() {
        unsafe {
            std::env::remove_var(MISSING_ENV);
        }
        let mut parsed = parsed_with_comment(&format!(r#"env("{MISSING_ENV}")"#));
        resolve_resource_refs_defer_unknown_functions(&mut parsed, &ProviderContext::default())
            .expect("permissive pass");

        let error = resolve_resource_refs_with_config(&mut parsed, &ProviderContext::default())
            .expect_err("strict pass must reject an unavailable unwrapped builtin");
        assert!(
            error
                .to_string()
                .contains(&format!("environment variable '{MISSING_ENV}' is not set")),
            "strict pass lost the original builtin diagnostic: {error}",
        );
    }

    #[test]
    fn secret_wraps_an_unavailable_value_without_losing_taint() {
        unsafe {
            std::env::remove_var(MISSING_ENV);
        }
        let mut parsed = parsed_with_comment(&format!(r#"secret(env("{MISSING_ENV}"))"#));
        resolve_resource_refs_defer_unknown_functions(&mut parsed, &ProviderContext::default())
            .expect("permissive pass");

        assert!(matches!(
            comment(&parsed),
            Value::Deferred(DeferredValue::Secret(inner))
                if matches!(inner.as_ref(), Value::Deferred(DeferredValue::Unavailable {
                    name: crate::builtins::BuiltinFunction::env,
                    ..
                }))
        ));
    }

    #[test]
    fn unavailable_reason_does_not_expose_an_env_derived_variable_name() {
        const NAME_SOURCE: &str = "CARINA_TEST_UNAVAILABLE_NAME_SOURCE_3974";
        const SENSITIVE_NAME: &str = "CARINA_TEST_SENSITIVE_VARIABLE_NAME_3974";
        unsafe {
            std::env::set_var(NAME_SOURCE, SENSITIVE_NAME);
            std::env::remove_var(SENSITIVE_NAME);
        }
        let mut parsed = parsed_with_comment(&format!(r#"env(env("{NAME_SOURCE}"))"#));
        resolve_resource_refs_defer_unknown_functions(&mut parsed, &ProviderContext::default())
            .expect("permissive pass");

        let Value::Deferred(DeferredValue::Unavailable { reason, .. }) = comment(&parsed) else {
            panic!(
                "nested env failure must be unavailable: {:?}",
                comment(&parsed)
            );
        };
        assert_eq!(
            reason.as_str(),
            "environment variable named by a sensitive value is not set"
        );
        assert!(!format!("{reason:?}").contains(SENSITIVE_NAME));

        unsafe {
            std::env::remove_var(NAME_SOURCE);
        }
    }
}

#[cfg(test)]
mod seed_precedence_tests {
    use super::*;
    use crate::parser::entry::SeedKind;

    #[test]
    fn structural_let_stays_out_of_value_model_and_seeds_as_typed_structural() {
        let parsed = crate::parser::parse(
            "let cluster = mock.compute.Instance { name = \"example\" }",
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse structural let");
        assert!(
            !parsed.variables.contains_key("cluster"),
            "structural handles must not enter the Value model"
        );

        let seeds = collect_seed_bindings_merged(&parsed);
        let cluster_seed = seeds
            .iter()
            .find(|seed| seed.name() == "cluster")
            .expect("cluster seed missing");

        match cluster_seed.kind() {
            SeedKind::TypedStructural(_)
            | SeedKind::Provisional
            | SeedKind::StructuralAlias(_)
            | SeedKind::ModuleParameter(_) => {}
            SeedKind::Value { value, .. } => {
                panic!("structural binding `cluster` must seed as Structural, not Value({value:?})")
            }
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
            SeedKind::Value { value, .. } => {
                assert_eq!(
                    value,
                    &Value::Concrete(ConcreteValue::String("literal".into())),
                    "Value seed must carry the actual literal"
                );
            }
            SeedKind::TypedStructural(_)
            | SeedKind::Provisional
            | SeedKind::StructuralAlias(_)
            | SeedKind::ModuleParameter(_) => {
                panic!("plain value let must seed as Value, not a non-value seed")
            }
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
        let crate::parser::DeferredForBody::Resource(template) =
            &parsed.parsed_deferred_for_expressions()[0].body
        else {
            panic!("deferred fixture body must be a resource")
        };
        assert!(matches!(
            template.attributes.get("comment"),
            Some(Value::Deferred(DeferredValue::FunctionCall { name, .. })) if name == "secret"
        ));

        resolve_resource_refs_with_config(&mut parsed, &ProviderContext::default())
            .expect("resolve deferred template functions");

        let crate::parser::DeferredForBody::Resource(template) =
            &parsed.parsed_deferred_for_expressions()[0].body
        else {
            panic!("deferred fixture body must be a resource")
        };
        assert!(matches!(
            template.attributes.get("comment"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        let parsed =
            crate::module_resolver::resolve_modules_providerless(parsed, std::path::Path::new("."))
                .expect("deferred fixture crosses the module-resolution seam");
        assert!(matches!(
            parsed.deferred_for_expressions()[0]
                .attributes
                .iter()
                .find_map(|(key, value)| (key == "comment").then_some(value)),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
    }
}
