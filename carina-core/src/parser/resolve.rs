//! Forward-reference resolution and identifier-scope checks.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ProviderContext;
use super::ast::{ExportParamLike, File, ParsedFile, ProviderConfig};
use super::entry::BindingSeed;
use super::error::{ParseError, undefined_identifier_error};
use super::static_eval::is_static_value;
use super::{ParsedDataSource as DataSource, ParsedResource as Resource, ParsedValue as Value};
use crate::resource::{
    AbsoluteBindingAddress, AccessMissPolicy, BindingAddress, ConcreteValue,
    ConcreteValue as GenericConcreteValue, DeferredValue, DiagnosticSourceLine, InterpolationPart,
    LoopSlot, NoPlaceholder, ParserPlaceholder, RelativeBindingName, UnavailableCoalescePolicy,
    UnavailableReason, UnavailableSecretPolicy, Value as GenericValue, find_unavailable_evaluation,
};
use crate::resource::{DiagnosticPath, DiagnosticScope, ReferencePath};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use thiserror::Error;

type RuntimeValue = GenericValue<NoPlaceholder, AbsoluteBindingAddress>;
type RuntimeProviderConfig = ProviderConfig<NoPlaceholder, AbsoluteBindingAddress>;

/// An analysis pass either rejects an unsealed input or reports an authored
/// semantic error. A semantic finding always has parser-owned provenance.
#[derive(Debug, thiserror::Error)]
pub enum AnalysisResolutionError {
    #[error(transparent)]
    Invariant(#[from] super::AnalysisTreeError),
    #[error(transparent)]
    Diagnostic(Box<super::Located<ParseError>>),
}

impl AnalysisResolutionError {
    fn from_parse_failure(error: ParseError) -> Self {
        match error {
            ParseError::AtElement(finding) => Self::Diagnostic(finding),
            error => Self::Invariant(super::AnalysisTreeError::new(
                super::AnalysisTreeErrorKind::MissingSyntaxProvenance {
                    element: format!("semantic failure: {error}"),
                },
            )),
        }
    }
}

fn provider_analysis_failure(
    locations: &[HashMap<String, super::DeclarationLocation>],
    index: usize,
    key: &str,
    error: ParseError,
) -> AnalysisResolutionError {
    if let ParseError::AtElement(finding) = error {
        return AnalysisResolutionError::Diagnostic(finding);
    }
    match locations
        .get(index)
        .and_then(|attributes| attributes.get(key))
    {
        Some(location) => AnalysisResolutionError::Diagnostic(Box::new(super::Located::new(
            location.clone(),
            error,
        ))),
        None => {
            super::AnalysisTreeError::new(super::AnalysisTreeErrorKind::MissingSyntaxProvenance {
                element: format!("provider {index} attribute {key}"),
            })
            .into()
        }
    }
}

trait ProviderAttributeBindingPhase: BindingAddress {
    fn evaluate_resolved_user_function(
        function: &super::UserFunction<Self>,
        functions: &std::sync::Arc<HashMap<String, super::UserFunction<Self>>>,
        arguments: &[crate::resource::FunctionArgument<ParserPlaceholder, Self>],
        config: &ProviderContext,
        session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
        call_line: usize,
    ) -> Result<GenericValue<ParserPlaceholder, Self>, ParseError>;
}

impl ProviderAttributeBindingPhase for RelativeBindingName {
    fn evaluate_resolved_user_function(
        function: &super::UserFunction<Self>,
        functions: &std::sync::Arc<HashMap<String, super::UserFunction<Self>>>,
        arguments: &[crate::resource::FunctionArgument<ParserPlaceholder, Self>],
        config: &ProviderContext,
        session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
        call_line: usize,
    ) -> Result<GenericValue<ParserPlaceholder, Self>, ParseError> {
        super::functions::evaluate_resolved_user_function(
            function, functions, arguments, config, session, call_line,
        )
    }
}

impl ProviderAttributeBindingPhase for AbsoluteBindingAddress {
    fn evaluate_resolved_user_function(
        function: &super::UserFunction<Self>,
        functions: &std::sync::Arc<HashMap<String, super::UserFunction<Self>>>,
        arguments: &[crate::resource::FunctionArgument<ParserPlaceholder, Self>],
        _config: &ProviderContext,
        session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
        call_line: usize,
    ) -> Result<GenericValue<ParserPlaceholder, Self>, ParseError> {
        match session {
            Some(session) => crate::resolver::evaluate_module_resolved_user_function(
                function, arguments, session, call_line,
            ),
            None => Ok(GenericValue::Deferred(
                DeferredValue::ResolvedFunctionCall {
                    function: std::sync::Arc::new(function.clone()),
                    functions: std::sync::Arc::clone(functions),
                    args: arguments.to_vec(),
                    source_line: call_line.into(),
                },
            )),
        }
    }
}

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
    config: RuntimeProviderConfig,
}

impl ResolvedProviderConfig {
    fn from_resolved(config: RuntimeProviderConfig) -> Self {
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

    pub fn attributes(&self) -> &IndexMap<String, RuntimeValue> {
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
    pub fn as_provider_config(&self) -> &RuntimeProviderConfig {
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
    provider_configs: Vec<RuntimeProviderConfig>,
}

impl ResolvedProviderConfigs {
    fn from_resolved(
        configs: Vec<RuntimeProviderConfig>,
    ) -> Result<Self, ProviderConfigInvariantError> {
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
        configs: Vec<RuntimeProviderConfig>,
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
    pub fn as_provider_configs(&self) -> &[RuntimeProviderConfig] {
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
#[serde(bound(
    serialize = "P: crate::resource::PlanValueKind",
    deserialize = "P: crate::resource::PlanValueKind"
))]
pub struct SavedProviderConfigs<P: crate::resource::PlanValueKind = crate::resource::SnapshotLeaf>(
    Vec<ProviderConfig<P, AbsoluteBindingAddress>>,
);

#[derive(Clone, Copy)]
enum ProviderConfigSection {
    Attribute,
    DefaultTag,
}

const PROVIDER_CONFIG_SECTIONS: [ProviderConfigSection; 2] = [
    ProviderConfigSection::Attribute,
    ProviderConfigSection::DefaultTag,
];

fn provider_config_section<P: crate::resource::ValueKind>(
    config: &ProviderConfig<P, AbsoluteBindingAddress>,
    section: ProviderConfigSection,
) -> &IndexMap<String, GenericValue<P>> {
    match section {
        ProviderConfigSection::Attribute => &config.attributes,
        ProviderConfigSection::DefaultTag => &config.default_tags,
    }
}

fn provider_config_section_mut<P: crate::resource::ValueKind>(
    config: &mut ProviderConfig<P, AbsoluteBindingAddress>,
    section: ProviderConfigSection,
) -> &mut IndexMap<String, GenericValue<P>> {
    match section {
        ProviderConfigSection::Attribute => &mut config.attributes,
        ProviderConfigSection::DefaultTag => &mut config.default_tags,
    }
}

fn provider_config_secret_location<P: crate::resource::ValueKind>(
    config: &ProviderConfig<P, AbsoluteBindingAddress>,
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

impl<P: crate::resource::PlanValueKind> SavedProviderConfigs<P> {
    pub fn as_provider_configs(&self) -> &[ProviderConfig<P, AbsoluteBindingAddress>] {
        &self.0
    }

    pub fn contains_redacted_secret(&self) -> bool {
        self.0.iter().any(|config| {
            config
                .attributes
                .values()
                .chain(config.default_tags.values())
                .chain(config.unresolved_attributes.values())
                .any(crate::value::contains_redacted_secret)
        })
    }

    pub fn redact_secrets(&self) -> Result<SavedProviderConfigs, crate::value::SerializationError> {
        let configs = self
            .0
            .iter()
            .map(|config| {
                config.try_map_value_stage(&mut |value| {
                    crate::value::redact_state_snapshot_value(
                        &crate::resource::state_value_snapshot(value),
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(SavedProviderConfigs(configs))
    }
}

impl SavedProviderConfigs {
    pub fn rehydrate_secrets(
        &self,
        evaluated: &Self,
    ) -> Result<SavedProviderConfigs<NoPlaceholder>, crate::value::SecretRehydrationSiteError> {
        let configs = self
            .0
            .iter()
            .map(|saved| {
                let candidate = evaluated.0.iter().find(|candidate| {
                    candidate.name == saved.name && candidate.binding == saved.binding
                });
                let mut out = saved.loading_projection();
                for section in PROVIDER_CONFIG_SECTIONS {
                    for (key, value) in provider_config_section(saved, section) {
                        let location = provider_config_secret_location(saved, section, key);
                        let candidate = candidate
                            .and_then(|candidate| {
                                provider_config_section(candidate, section).get(key)
                            })
                            .map(|value| value.to_value_at(&location))
                            .transpose()?;
                        let restored = crate::value::rehydrate_secret_entry(
                            value,
                            candidate.as_ref(),
                            location.clone(),
                            crate::value::SecretRehydrationCandidate::Evaluated,
                        )?;
                        let restored = match restored {
                            Some(value) => value,
                            None => value.to_value_at(location)?,
                        };
                        provider_config_section_mut(&mut out, section)
                            .insert(key.clone(), restored);
                    }
                }
                Ok(out)
            })
            .collect::<Result<_, crate::value::SecretRehydrationSiteError>>()?;
        Ok(SavedProviderConfigs(configs))
    }
}

impl From<&ResolvedProviderConfigs> for SavedProviderConfigs {
    fn from(configs: &ResolvedProviderConfigs) -> Self {
        Self(
            configs
                .provider_configs
                .iter()
                .map(|config| {
                    config.try_map_value_stage(&mut |value| {
                        Ok::<_, std::convert::Infallible>(value.to_snapshot())
                    })
                })
                .collect::<Result<_, _>>()
                .expect("infallible snapshot projection"),
        )
    }
}

impl<P: crate::resource::PlanValueKind> TryFrom<SavedProviderConfigs<P>>
    for ResolvedProviderConfigs
{
    type Error = ProviderConfigInvariantError;
    fn try_from(configs: SavedProviderConfigs<P>) -> Result<Self, Self::Error> {
        let configs = configs
            .0
            .into_iter()
            .map(|config| {
                config.try_map_value_stage(&mut |value| {
                    crate::resource::state_value_snapshot(value)
                        .to_value_at(format!("provider {}", config.name))
                })
            })
            .collect::<Result<_, _>>()
            .map_err(ProviderConfigInvariantError::UnavailableSecret)?;
        Self::from_resolved(configs)
    }
}

#[cfg(test)]
mod saved_provider_config_secret_tests {
    use super::*;
    use crate::resource::DeferredValue as GenericDeferredValue;

    fn secret(value: &str) -> RuntimeValue {
        GenericValue::Deferred(GenericDeferredValue::Secret(
            Box::new(GenericValue::Concrete(GenericConcreteValue::String(
                value.into(),
            )))
            .into(),
        ))
    }

    fn provider(token: &str) -> RuntimeProviderConfig {
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

    fn snapshot_configs(configs: &SavedProviderConfigs<NoPlaceholder>) -> SavedProviderConfigs {
        SavedProviderConfigs(
            configs
                .0
                .iter()
                .map(|config| {
                    config
                        .try_map_value_stage(&mut |value| {
                            Ok::<_, std::convert::Infallible>(value.to_snapshot())
                        })
                        .expect("infallible saved config projection")
                })
                .collect(),
        )
    }

    fn placeholder_hash(
        value: &crate::resource::SnapshotValue,
    ) -> &crate::resource::RedactedSecretHash {
        match value {
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                crate::resource::SnapshotLeaf::RedactedSecret { hash },
            )) => hash,
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
                .rehydrate_secrets(&snapshot_configs(&evaluated))
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
            .rehydrate_secrets(&snapshot_configs(&SavedProviderConfigs(vec![mismatched])))
            .expect_err("a placeholder must map to its provider instance");
        assert!(matches!(
            error.source,
            crate::value::SecretRehydrationError::HashMismatch
                | crate::value::SecretRehydrationError::ShapeMismatch
        ));
    }
}

fn validate_resolved_provider_configs(
    configs: &[RuntimeProviderConfig],
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
    #[error(transparent)]
    UnavailableSecret(#[from] crate::resource::UnavailableSecretError),

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
    fn dependency_bindings(&self) -> &BTreeSet<RelativeBindingName>;
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value>;
    fn directives_mut(&mut self) -> &mut crate::resource::AuthoredDirectives;
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<RelativeBindingName>;
    fn declaration(&self) -> &super::DeclarationProvenance;
}

impl PreApplyResourceMut for Resource {
    fn declaration(&self) -> &super::DeclarationProvenance {
        &self.declaration
    }
    fn id(&self) -> &crate::resource::ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<RelativeBindingName> {
        &self.dependencies.value_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn directives_mut(&mut self) -> &mut crate::resource::AuthoredDirectives {
        &mut self.directives
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<RelativeBindingName> {
        &mut self.dependencies.value_bindings
    }
}

impl PreApplyResourceMut for DataSource {
    fn declaration(&self) -> &super::DeclarationProvenance {
        &self.declaration
    }
    fn id(&self) -> &crate::resource::ResourceId {
        &self.id
    }
    fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
    fn dependency_bindings(&self) -> &BTreeSet<RelativeBindingName> {
        &self.dependencies.value_bindings
    }
    fn attributes_mut(&mut self) -> &mut IndexMap<String, Value> {
        &mut self.attributes
    }
    fn directives_mut(&mut self) -> &mut crate::resource::AuthoredDirectives {
        &mut self.directives
    }
    fn dependency_bindings_mut(&mut self) -> &mut BTreeSet<RelativeBindingName> {
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
pub fn resolve_resource_refs(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_config(parsed, config)
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
    resolve_resource_refs_with_policy(parsed, config, None, false, false)
}

/// Resolve pending checks with the same session used for declaration checks.
pub fn resolve_resource_refs_in_session<E: ExportParamLike>(
    parsed: &mut File<E>,
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), AnalysisResolutionError> {
    super::AnalysisTree::try_from_file(parsed)?;
    resolve_resource_refs_with_policy(parsed, session.catalog(), Some(session), false, false)
        .map_err(AnalysisResolutionError::from_parse_failure)
}

/// Resolve a copy of the already sealed input. The original remains a valid
/// read-only analysis view; no independently supplied raw file can bypass it.
///
/// ```compile_fail,E0308
/// use carina_core::parser::{ParsedFile, resolve_resource_refs_from_analysis};
/// use carina_core::validation::deferred_types::ProviderTypeCheckContext;
/// fn resolve(file: &ParsedFile, session: &dyn ProviderTypeCheckContext) {
///     resolve_resource_refs_from_analysis(file, session);
/// }
/// ```
pub fn resolve_resource_refs_from_analysis<E: ExportParamLike + Clone>(
    tree: &super::AnalysisTree<
        '_,
        ParserPlaceholder,
        ParserPlaceholder,
        super::ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>,
        RelativeBindingName,
        E,
    >,
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<File<E>, AnalysisResolutionError> {
    let mut parsed = tree.file().clone();
    resolve_resource_refs_with_policy(&mut parsed, session.catalog(), Some(session), false, false)
        .map_err(AnalysisResolutionError::from_parse_failure)?;
    Ok(parsed)
}

/// Parser/module-loader evaluation before the analysis boundary is sealed.
pub(crate) fn resolve_resource_refs_in_session_unsealed<E: ExportParamLike>(
    parsed: &mut File<E>,
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<(), ParseError> {
    resolve_resource_refs_with_policy(parsed, session.catalog(), Some(session), false, false)
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

    let mut fn_ctx = super::ParseContext::for_evaluation(config);
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
            *value =
                resolve_function_calls_only(value, &binding_map, fn_ctx.config, None, true, false)?;
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
    resolve_resource_refs_with_policy(parsed, config, None, true, false)
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
    resolve_resource_refs_with_policy(parsed, config, None, false, true)
}

fn resolve_resource_refs_with_policy<E: ExportParamLike>(
    parsed: &mut File<E>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<(), ParseError> {
    let syntax = parsed.parser_binding_metadata.syntax().clone();
    let locate_named = |kind, name: &str, error: ParseError| match syntax
        .declarations()
        .iter()
        .rev()
        .find(|record| record.kind() == kind && record.name() == name)
    {
        Some(record) => error.at(record.value_location().unwrap_or(record.location())),
        None => error,
    };
    if let Some(session) = session {
        for check in parsed
            .parser_binding_metadata
            .pending_function_type_checks()
        {
            super::functions::validate_pending_function_type_check(
                check,
                &parsed.user_functions,
                session,
            )
            .map_err(|error| error.at(&check.location))?;
        }
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
        let mut deps: std::collections::HashSet<RelativeBindingName> =
            std::collections::HashSet::new();
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
            binding_map.entry(name.to_string()).or_default();
        }
    }

    // Register upstream_state bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved at plan time when the state file is loaded.
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.to_string()).or_default();
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
    let mut fn_ctx = super::ParseContext::for_evaluation(config);
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
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| {
                match syntax
                    .backend_keys()
                    .iter()
                    .rev()
                    .find(|record| record.detail() == key)
                {
                    Some(record) => error.at(record.location()),
                    None => error,
                }
            })?;
            if crate::value::contains_secret(&evaluated) {
                let span = backend.attribute_spans.get(key).copied().ok_or_else(|| {
                    ParseError::InternalError {
                        expected: format!("source span for backend attribute '{key}'"),
                        context: "secret-derived backend attribute validation".to_string(),
                    }
                })?;
                let error = ParseError::SecretDerivedBackendAttribute {
                    backend_type: backend.backend_type.clone(),
                    attribute: key.clone(),
                    line: span.start_line(),
                    start_column: span.start_column(),
                    end_column: span.end_column(),
                };
                return Err(
                    match syntax
                        .backend_keys()
                        .iter()
                        .rev()
                        .find(|record| record.detail() == key)
                    {
                        Some(record) => error.at(record.location()),
                        None => error,
                    },
                );
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
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| match resource.declaration().location() {
                Some(location) => error.at(location),
                None => match &source_file {
                    Some(path) => error.in_file(path.clone()),
                    None => error,
                },
            })?;
            resolved_attrs.insert(key.clone(), resolved);
        }

        *resource.attributes_mut() = resolved_attrs;
        resolve_directive_function_calls(
            resource.directives_mut(),
            &binding_map,
            fn_ctx.config,
            session,
            defer_unknown_functions,
            preserve_failed_user_calls,
        )?;
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
        let location = match deferred.body() {
            super::DeferredForBody::Resource(resource) => {
                resource.as_resource().declaration.location().cloned()
            }
            super::DeferredForBody::Read(source) => source.declaration.location().cloned(),
            super::DeferredForBody::ModuleCall(call) => {
                Some(call.id().declaration_location().clone())
            }
        };
        match &mut deferred.body {
            crate::parser::DeferredForBody::Resource(resource) => resolve_directive_function_calls(
                resource.directives_mut(),
                &binding_map,
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?,
            crate::parser::DeferredForBody::Read(source) => resolve_directive_function_calls(
                &mut source.directives,
                &binding_map,
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )?,
            crate::parser::DeferredForBody::ModuleCall(_) => {}
        }
        let Some(attributes) = deferred.template_attributes_mut() else {
            continue;
        };
        for value in attributes.values_mut() {
            *value = resolve_function_calls_only(
                value,
                &binding_map,
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| match &location {
                Some(location) => error.at(location),
                None => error,
            })?;
        }
    }

    for conditional in &mut parsed.deferred_module_ifs {
        for body in conditional.branches_mut() {
            match body {
                crate::parser::DeferredIfBody::Resource(resource) => {
                    resolve_directive_function_calls(
                        &mut resource.directives,
                        &binding_map,
                        fn_ctx.config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?
                }
                crate::parser::DeferredIfBody::DataSource(source) => {
                    resolve_directive_function_calls(
                        &mut source.directives,
                        &binding_map,
                        fn_ctx.config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?
                }
                crate::parser::DeferredIfBody::ModuleCall(_) => {}
            }
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
            fn_ctx.config,
            session,
            defer_unknown_functions,
            preserve_failed_user_calls,
        )
        .map_err(|error| {
            locate_named(crate::config_loader::DeclarationKind::Binding, &key, error)
        })?;
        parsed.variables[&key] = resolved;
    }

    // Resolve calls in every other value-bearing declaration as well. These
    // expressions may name a user function declared in a sibling `.crn`
    // file, so the per-file parser necessarily leaves a FunctionCall behind.
    // Type checking module arguments, attributes, or exports before this pass
    // would treat that deferred call as unknown and silently accept it.
    for call in &mut parsed.module_calls {
        let location = call.id().declaration_location().clone();
        for argument in call.arguments_mut() {
            argument.try_rewrite(|value| {
                resolve_function_calls_only(
                    value,
                    &binding_map,
                    fn_ctx.config,
                    session,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                )
                .map_err(|error| error.at(&location))
            })?;
        }
    }
    for argument in &mut parsed.arguments {
        if let Some(default) = &mut argument.default {
            *default = resolve_function_calls_only(
                default,
                &binding_map,
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| {
                locate_named(
                    crate::config_loader::DeclarationKind::Argument,
                    &argument.name,
                    error,
                )
            })?;
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
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| {
                locate_named(
                    crate::config_loader::DeclarationKind::ModuleAttribute,
                    &attribute.name,
                    error,
                )
            })?;
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
                fn_ctx.config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
            )
            .map_err(|error| {
                locate_named(
                    crate::config_loader::DeclarationKind::Export,
                    export.name(),
                    error,
                )
            })?;
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
    metadata: &'a impl super::ast::BindingSeedMetadata,
) -> Vec<BindingSeed<'a>> {
    let mut names: Vec<&'a str> = structural.iter().copied().collect();
    names.extend(variables.keys().map(String::as_str));
    names.extend(
        metadata
            .structural_handles()
            .keys()
            .map(BindingAddress::as_str),
    );
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
                match metadata.value_structural_handles_by_name(name) {
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
) -> Option<Vec<DiagnosticPath<RelativeBindingName>>> {
    let mut visited = HashSet::new();
    let mut visiting = Vec::new();

    for name in variables.keys() {
        if structural.contains(name.as_str()) {
            continue;
        }
        if let Some(chain) = visit_let_binding(
            name,
            DiagnosticPath::Public(ReferencePath::Binding(name.as_str().into())),
            variables,
            structural,
            &mut visited,
            &mut visiting,
        ) {
            return Some(chain);
        }
    }

    None
}

fn visit_let_binding(
    name: &str,
    diagnostic_path: DiagnosticPath<RelativeBindingName>,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    visited: &mut HashSet<String>,
    visiting: &mut Vec<(String, DiagnosticPath<RelativeBindingName>)>,
) -> Option<Vec<DiagnosticPath<RelativeBindingName>>> {
    if visited.contains(name) {
        return None;
    }
    if let Some(pos) = visiting.iter().position(|(candidate, _)| candidate == name) {
        let mut cycle = visiting[pos..]
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<Vec<_>>();
        cycle.push(diagnostic_path);
        if cycle
            .iter()
            .any(|path| matches!(path, DiagnosticPath::Protected))
        {
            cycle.fill(DiagnosticPath::Protected);
        }
        return Some(cycle);
    }

    let value = variables.get(name)?;
    visiting.push((name.to_string(), diagnostic_path));

    let roots = reference_roots_in_value(value, variables, structural);
    for (root, path) in roots {
        if let Some(chain) =
            visit_let_binding(&root, path, variables, structural, visited, visiting)
        {
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
) -> Vec<(String, DiagnosticPath<RelativeBindingName>)> {
    let mut roots = Vec::new();
    collect_reference_roots(
        value,
        variables,
        structural,
        DiagnosticScope::Public,
        &mut roots,
    );
    roots
}

fn collect_reference_roots(
    value: &Value,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    scope: DiagnosticScope,
    roots: &mut Vec<(String, DiagnosticPath<RelativeBindingName>)>,
) {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            for nested in deferred.values() {
                collect_reference_roots(nested, variables, structural, scope, roots);
            }
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(name)) => {
            // Bare-identifier fallback (`EnumIdentifier("B")`) and dotted-access
            // fallback (`EnumIdentifier("B.attr")`) both appear before the parser
            // classifies the RHS as a binding ref or resource ref.
            push_root(name.as_str(), variables, structural, scope, roots);
            if let Some((root, _)) = name.as_str().split_once('.') {
                push_root(root, variables, structural, scope, roots);
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                collect_reference_roots(item, variables, structural, scope, roots);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for item in map.values() {
                collect_reference_roots(item, variables, structural, scope, roots);
            }
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            push_root(binding, variables, structural, scope, roots);
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            push_root(path.binding(), variables, structural, scope, roots);
        }
        GenericValue::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            for arg in args {
                collect_reference_roots(arg.walker_value(), variables, structural, scope, roots);
            }
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            collect_reference_roots(access.base(), variables, structural, scope, roots);
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let InterpolationPart::Expr(expr) = part {
                    collect_reference_roots(expr, variables, structural, scope, roots);
                }
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            inner.visit_reference_inventory(|value, scope| {
                collect_reference_roots(value, variables, structural, scope, roots)
            });
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            collect_reference_roots(value, variables, structural, scope, roots);
            collect_reference_roots(default, variables, structural, scope, roots);
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            collect_reference_roots(expression.lhs(), variables, structural, scope, roots);
            collect_reference_roots(expression.rhs(), variables, structural, scope, roots);
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
        | Value::Deferred(DeferredValue::Unknown(_))
        | Value::Deferred(DeferredValue::Placeholder(_)) => {}
    }
}

fn push_root(
    root: &str,
    variables: &IndexMap<String, Value>,
    structural: &HashSet<&str>,
    scope: DiagnosticScope,
    roots: &mut Vec<(String, DiagnosticPath<RelativeBindingName>)>,
) {
    if variables.contains_key(root) && !structural.contains(root) {
        if let Some((_, path)) = roots.iter_mut().find(|(name, _)| name == root) {
            if scope.is_secret() {
                *path = DiagnosticPath::Protected;
            }
        } else {
            roots.push((
                root.to_string(),
                scope.path(ReferencePath::Binding(root.into())),
            ));
        }
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
pub use crate::validation::routing::check_provider_instance_routing;

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
            errors.push(undefined_identifier_error(
                known,
                root.to_string(),
                deferred.line(),
            ));
        }
    }
}

fn resolve_value_with_config<B: ProviderAttributeBindingPhase>(
    value: &GenericValue<ParserPlaceholder, B>,
    binding_map: &HashMap<String, HashMap<String, GenericValue<ParserPlaceholder, B>>>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<GenericValue<ParserPlaceholder, B>, ParseError> {
    resolve_value_with_binding_lookup(
        value,
        &|binding: &B, attribute: &str| {
            binding_map
                .get(binding.as_str())
                .and_then(|attributes| attributes.get(attribute))
                .cloned()
        },
        config,
        session,
        defer_unknown_functions,
        preserve_failed_user_calls,
        DiagnosticScope::Public,
    )
}

fn resolve_value_with_binding_lookup<B: ProviderAttributeBindingPhase>(
    value: &GenericValue<ParserPlaceholder, B>,
    binding_lookup: &impl Fn(&B, &str) -> Option<GenericValue<ParserPlaceholder, B>>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
    scope: DiagnosticScope,
) -> Result<GenericValue<ParserPlaceholder, B>, ParseError> {
    match value {
        GenericValue::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let deferred = deferred.try_map(|value| {
                resolve_value_with_binding_lookup(
                    value,
                    binding_lookup,
                    config,
                    session,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                    scope,
                )
            })?;
            Ok(GenericValue::Deferred(DeferredValue::ModuleIf(Box::new(
                deferred,
            ))))
        }
        GenericValue::Deferred(DeferredValue::Coalesce {
            value: left,
            default,
        }) => {
            let left = resolve_value_with_binding_lookup(
                left,
                binding_lookup,
                config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
                scope,
            )?;
            if matches!(left, GenericValue::Concrete(_)) {
                Ok(left)
            } else {
                // Carry the fallback through the same directory fixed point as
                // the left operand, while preserving failures as deferred calls
                // until absence actually selects it.
                let default = resolve_value_with_binding_lookup(
                    default,
                    binding_lookup,
                    config,
                    session,
                    true,
                    true,
                    scope,
                )?;
                Ok(GenericValue::Deferred(DeferredValue::Coalesce {
                    value: Box::new(left),
                    default: Box::new(default),
                }))
            }
        }
        GenericValue::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = resolve_value_with_binding_lookup(
                expression.lhs(),
                binding_lookup,
                config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
                scope,
            )?;
            let rhs = resolve_value_with_binding_lookup(
                expression.rhs(),
                binding_lookup,
                config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
                scope,
            )?;
            let span = expression.operator_span();
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ParseError::integer_arithmetic(span, 0, error))
        }
        GenericValue::Deferred(DeferredValue::ResourceRef { path }) => {
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
            match binding_lookup(path.binding_address(), path.attribute()) {
                Some(attribute_value) => resolve_value_with_binding_lookup(
                    &attribute_value,
                    binding_lookup,
                    config,
                    session,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                    scope,
                ),
                // A missing binding or attribute may become available later.
                // The directory-wide scope check owns undefined identifiers.
                None => Ok(value.clone()),
            }
        }
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<_>, ParseError> = items
                .iter()
                .map(|item| {
                    resolve_value_with_binding_lookup(
                        item,
                        binding_lookup,
                        config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                        scope,
                    )
                })
                .collect();
            Ok(GenericValue::Concrete(ConcreteValue::List(resolved?)))
        }
        GenericValue::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_value_with_binding_lookup(
                        v,
                        binding_lookup,
                        config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                        scope,
                    )?,
                );
            }
            Ok(GenericValue::Concrete(ConcreteValue::Map(resolved)))
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved: Result<Vec<_>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_value_with_binding_lookup(
                            v,
                            binding_lookup,
                            config,
                            session,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                            scope,
                        )?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            let resolved = resolved?;
            reject_required_interpolation_unavailable(&resolved, defer_unknown_functions)?;
            Ok(GenericValue::Deferred(DeferredValue::Interpolation(resolved)).canonicalize())
        }
        GenericValue::Deferred(DeferredValue::Unavailable {
            name,
            reason,
            source_line,
            ..
        }) => {
            // External inputs can remain unavailable below a secret until
            // planning/apply. Deterministic builtin failures still fail validation.
            if defer_unknown_functions
                || (scope.is_secret()
                    && matches!(
                        name,
                        crate::builtins::BuiltinFunction::env
                            | crate::builtins::BuiltinFunction::decrypt
                    ))
            {
                Ok(value.clone())
            } else {
                Err(unavailable_builtin_error(*name, reason, *source_line))
            }
        }
        GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
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
                        resolve_value_with_binding_lookup(
                            value,
                            binding_lookup,
                            config,
                            session,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                            scope,
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let evaluated = B::evaluate_resolved_user_function(
                function,
                functions,
                &resolved_args,
                config,
                session,
                evaluation_line,
            );
            if preserve_failed_user_calls && evaluated.is_err() {
                return Ok(value.clone());
            }
            evaluated
        }
        GenericValue::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let authored_line = source_line.get();
            let resolved_args: Result<Vec<_>, ParseError> = args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        resolve_value_with_binding_lookup(
                            value,
                            binding_lookup,
                            config,
                            session,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                            scope,
                        )
                    })
                })
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(|argument| {
                argument.handle().is_some() || argument.value().is_some_and(is_static_value)
            });

            let eval_args: Vec<_> = resolved_args
                .iter()
                .map(super::functions::function_argument_as_eval)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name.as_str(), &eval_args, config) {
                Ok(result) => result.into_value().map_err(|error| match authored_line {
                    Some(line) => ParseError::from_eval_value(error, line),
                    None => ParseError::UserFunctionError(error.to_string()),
                }),
                Err(e) => {
                    if all_args_resolved {
                        let reason = if scope.is_secret() {
                            UnavailableReason::inside_secret()
                        } else {
                            UnavailableReason::from_builtin_error(e)
                        };
                        if defer_unknown_functions {
                            Ok(GenericValue::Deferred(DeferredValue::Unavailable {
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
                        Ok(GenericValue::Deferred(DeferredValue::FunctionCall {
                            name: *name,
                            args: resolved_args,
                            source_line: *source_line,
                        }))
                    }
                }
            }
        }
        GenericValue::Deferred(DeferredValue::Access(access)) => {
            let resolved_base = resolve_value_with_binding_lookup(
                access.base(),
                binding_lookup,
                config,
                session,
                defer_unknown_functions,
                preserve_failed_user_calls,
                scope,
            )?;
            access
                .rebuild_with_base(resolved_base, AccessMissPolicy::Error)
                .map_err(|error| ParseError::InvalidExpression {
                    line: access.source_line(),
                    message: error.to_string(),
                })
        }
        GenericValue::Deferred(DeferredValue::Secret(inner)) => inner
            .try_map(|value| {
                resolve_value_with_binding_lookup(
                    value,
                    binding_lookup,
                    config,
                    session,
                    defer_unknown_functions,
                    preserve_failed_user_calls,
                    DiagnosticScope::Secret,
                )
            })
            .map(|inner| GenericValue::Deferred(DeferredValue::Secret(inner))),
        GenericValue::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | GenericValue::Deferred(
            DeferredValue::BindingRef { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
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

fn reject_required_interpolation_unavailable<B: BindingAddress>(
    parts: &[InterpolationPart<ParserPlaceholder, B>],
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

fn resolve_directive_function_calls(
    directives: &mut crate::resource::AuthoredDirectives,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<(), ParseError> {
    directives.try_update_expressions(|_, expression| {
        expression.value = resolve_function_calls_only(
            &expression.value,
            binding_map,
            config,
            session,
            defer_unknown_functions,
            preserve_failed_user_calls,
        )
        .map_err(|error| match expression.source().declaration_location() {
            Some(location) => error.at(location),
            None => error,
        })?;
        Ok(())
    })
}

/// Walk a value tree and finalize any `Value::Deferred(DeferredValue::FunctionCall)` placeholder
/// that the per-file parse deferred (typically because the user-fn
/// lives in a sibling `.crn` — #2444). Recurses through `List` / `Map`
/// / `Interpolation` containers so deferred calls inside them surface
/// too. Non-FunctionCall arms (`String`, `ResourceRef`,
/// `Interpolation` literals, etc.) pass through unchanged so earlier
/// passes that read raw forms keep working.
fn resolve_function_calls_only<B: ProviderAttributeBindingPhase>(
    value: &GenericValue<ParserPlaceholder, B>,
    binding_map: &HashMap<String, HashMap<String, GenericValue<ParserPlaceholder, B>>>,
    config: &ProviderContext,
    session: Option<&dyn crate::validation::deferred_types::ProviderTypeCheckContext>,
    defer_unknown_functions: bool,
    preserve_failed_user_calls: bool,
) -> Result<GenericValue<ParserPlaceholder, B>, ParseError> {
    match value {
        GenericValue::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Coalesce { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Secret(_),
        ) => resolve_value_with_config(
            value,
            binding_map,
            config,
            session,
            defer_unknown_functions,
            preserve_failed_user_calls,
        ),
        GenericValue::Concrete(ConcreteValue::List(items)) => {
            let resolved: Result<Vec<_>, ParseError> = items
                .iter()
                .map(|v| {
                    resolve_function_calls_only(
                        v,
                        binding_map,
                        config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )
                })
                .collect();
            Ok(GenericValue::Concrete(ConcreteValue::List(resolved?)))
        }
        GenericValue::Concrete(ConcreteValue::Map(map)) => {
            let mut resolved = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_function_calls_only(
                        v,
                        binding_map,
                        config,
                        session,
                        defer_unknown_functions,
                        preserve_failed_user_calls,
                    )?,
                );
            }
            Ok(GenericValue::Concrete(ConcreteValue::Map(resolved)))
        }
        GenericValue::Deferred(DeferredValue::Interpolation(parts)) => {
            let resolved: Result<Vec<_>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => {
                        Ok(InterpolationPart::Expr(resolve_function_calls_only(
                            v,
                            binding_map,
                            config,
                            session,
                            defer_unknown_functions,
                            preserve_failed_user_calls,
                        )?))
                    }
                    other => Ok(other.clone()),
                })
                .collect();
            let resolved = resolved?;
            reject_required_interpolation_unavailable(&resolved, defer_unknown_functions)?;
            Ok(GenericValue::Deferred(DeferredValue::Interpolation(resolved)).canonicalize())
        }
        GenericValue::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | GenericValue::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => Ok(value.clone()),
    }
}

pub fn resolve_provider_unresolved_attributes_for_analysis<
    E: ExportParamLike<ParserPlaceholder, RelativeBindingName>,
    D,
>(
    parsed: &mut File<E, ParserPlaceholder, ParserPlaceholder, D, RelativeBindingName>,
    config: &ProviderContext,
) -> Result<(), AnalysisResolutionError>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, RelativeBindingName>,
{
    let locations = super::AnalysisTree::try_from_file(parsed)?
        .provider_attribute_locations()
        .to_vec();
    resolve_provider_unresolved_attributes_mapped(parsed, config, |index, key, error| {
        provider_analysis_failure(&locations, index, key, error)
    })
}

pub fn resolve_expanded_provider_unresolved_attributes_for_analysis<
    E: ExportParamLike<ParserPlaceholder, AbsoluteBindingAddress>,
    D,
>(
    parsed: &mut File<E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>,
    config: &ProviderContext,
) -> Result<(), AnalysisResolutionError>
where
    D: super::DeferredForPhaseTraversal<
            ParserPlaceholder,
            ParserPlaceholder,
            AbsoluteBindingAddress,
        >,
{
    let locations = super::AnalysisTree::try_from_file(parsed)?
        .provider_attribute_locations()
        .to_vec();
    resolve_provider_unresolved_attributes_mapped(parsed, config, |index, key, error| {
        provider_analysis_failure(&locations, index, key, error)
    })
}

pub fn finalize_provider_configs_for_analysis<
    E: ExportParamLike<ParserPlaceholder, B>,
    B: BindingAddress,
    D,
>(
    parsed: &mut File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> Result<(), AnalysisResolutionError>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let locations = super::AnalysisTree::try_from_file(parsed)?
        .provider_attribute_locations()
        .to_vec();
    finalize_provider_configs_mapped(parsed, |index, key, error| {
        provider_analysis_failure(&locations, index, key, error)
    })
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
    parsed: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, RelativeBindingName>,
    config: &ProviderContext,
) -> Result<(), ParseError>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, RelativeBindingName>,
{
    resolve_provider_unresolved_attributes_for_phase(parsed, config)
}

/// Resolve deferred provider attributes after root/module binding expansion.
pub fn resolve_expanded_provider_unresolved_attributes<E, D>(
    parsed: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, AbsoluteBindingAddress>,
    config: &ProviderContext,
) -> Result<(), ParseError>
where
    D: super::DeferredForPhaseTraversal<
            ParserPlaceholder,
            ParserPlaceholder,
            AbsoluteBindingAddress,
        >,
{
    resolve_provider_unresolved_attributes_for_phase(parsed, config)
}

fn resolve_provider_unresolved_attributes_for_phase<E, B: ProviderAttributeBindingPhase, D>(
    parsed: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &ProviderContext,
) -> Result<(), ParseError>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resolve_provider_unresolved_attributes_mapped(parsed, config, |_, _, error| error)
}

fn resolve_provider_unresolved_attributes_mapped<E, B: ProviderAttributeBindingPhase, D, Failure>(
    parsed: &mut super::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    config: &ProviderContext,
    mut map_error: impl FnMut(usize, &str, ParseError) -> Failure,
) -> Result<(), Failure>
where
    D: super::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut binding_map: HashMap<String, HashMap<String, GenericValue<ParserPlaceholder, B>>> =
        HashMap::new();
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
            binding_map.entry(name.to_string()).or_default();
        }
    }
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.to_string()).or_default();
    }
    for wb in &parsed.wait_bindings {
        binding_map
            .entry(wb.binding.as_str().to_string())
            .or_default();
    }
    for (index, provider) in parsed.providers.iter_mut().enumerate() {
        let mut resolved = IndexMap::new();
        for (key, expr) in &provider.unresolved_attributes {
            let r = resolve_value_with_config(expr, &binding_map, config, None, false, false)
                .map_err(|error| map_error(index, key, error))?;
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
    parsed: &super::File<E, NoPlaceholder, LoopSlot, D, AbsoluteBindingAddress>,
    remote_bindings: &HashMap<String, HashMap<String, RuntimeValue>>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<ResolvedProviderConfigs, ProviderConfigResolutionError> {
    let mut bindings =
        crate::binding_index::ResolvedBindings::<NoPlaceholder, AbsoluteBindingAddress>::default();
    let mut parser_bindings = HashMap::new();
    for rref in parsed.iter_top_level_resources() {
        if let Some(binding_name) = rref.binding() {
            let attributes = match rref {
                super::TopLevelResourceRef::Resource(resource) => resource.resolved_attributes(),
                super::TopLevelResourceRef::DataSource(data_source) => {
                    crate::resource::attrs_to_hashmap(&data_source.attributes)
                }
                super::TopLevelResourceRef::LiveComposition(composition) => composition
                    .signature
                    .attributes
                    .iter()
                    .filter_map(|(name, attribute)| {
                        attribute.value().map(|value| (name.clone(), value))
                    })
                    .collect(),
            };
            parser_bindings.insert(binding_name.to_string(), attributes.clone());
            bindings.set_with_completeness(
                binding_name,
                attributes,
                crate::binding_index::BindingValueSource::Local,
                crate::binding_index::BindingCompleteness::Incomplete,
            );
        }
    }
    for (binding, attrs) in remote_bindings {
        parser_bindings.insert(binding.clone(), attrs.clone());
        bindings.set_with_completeness(
            binding,
            attrs.clone(),
            crate::binding_index::BindingValueSource::Upstream,
            crate::binding_index::BindingCompleteness::Complete,
        );
    }

    let mut providers = Vec::with_capacity(parsed.providers.len());
    for provider in &parsed.providers {
        let mut provider = provider.clone();
        for section in [&mut provider.attributes, &mut provider.default_tags] {
            let mut resolved = IndexMap::new();
            for (key, expr) in section.iter() {
                // Evaluate authored functions through the typed parse-error
                // path before the runtime resolver's string diagnostic seam.
                // Only the placeholder stage changes; addresses stay absolute.
                let parser_expr = expr.clone().into_placeholder_type::<ParserPlaceholder>();
                let parser_resolved = resolve_value_with_binding_lookup(
                    &parser_expr,
                    &|binding: &AbsoluteBindingAddress, attribute: &str| {
                        parser_bindings
                            .get(binding.as_str())
                            .and_then(|attributes| attributes.get(attribute))
                            .cloned()
                            .map(RuntimeValue::into_placeholder_type)
                    },
                    config.catalog(),
                    Some(config),
                    false,
                    false,
                    DiagnosticScope::Public,
                )?;
                let expanded = parser_resolved.into_expanded().map_err(|error| {
                    ParseError::InvalidExpression {
                        line: 0,
                        message: error.to_string(),
                    }
                })?;
                let value =
                    crate::resolver::resolve_ref_value_with_config(&expanded, &bindings, config)
                        .map_err(|message| ParseError::InvalidExpression {
                            line: 0,
                            message: message.to_string(),
                        })?;
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
pub fn finalize_provider_configs<E, B: BindingAddress, D>(
    parsed: &mut super::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
) -> Result<(), ParseError> {
    finalize_provider_configs_mapped(parsed, |_, _, error| error)
}

fn finalize_provider_configs_mapped<E, B: BindingAddress, D, Failure>(
    parsed: &mut super::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    mut map_error: impl FnMut(usize, &str, ParseError) -> Failure,
) -> Result<(), Failure> {
    for (index, provider) in parsed.providers.iter_mut().enumerate() {
        if let Some(value) = provider.unresolved_attributes.shift_remove("default_tags") {
            match value {
                GenericValue::Concrete(GenericConcreteValue::Map(tags)) => {
                    provider.default_tags = tags;
                }
                other => {
                    return Err(map_error(
                        index,
                        "default_tags",
                        ParseError::InvalidExpression {
                            line: 0,
                            message: format!(
                                "Provider '{}': default_tags must resolve to a map, got {other:?}",
                                provider.name
                            ),
                        },
                    ));
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
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

        resolve_resource_refs_with_config(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
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
        parsed_with_comment_and_config(
            expression,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
    }

    fn parsed_with_comment_and_config(expression: &str, config: &ProviderContext) -> ParsedFile {
        crate::parser::parse(
            &format!(
                r#"
let item = mock.test.resource {{
  name    = "unavailable-evaluation"
  comment = {expression}
}}
"#,
            ),
            config,
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
        let mut failed = parsed_with_comment(&format!(r#"env("{MISSING_ENV}")"#));
        resolve_resource_refs_defer_unknown_functions(
            &mut failed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("parse pending evaluation fixture");
        resolve_resource_refs_defer_unknown_functions(
            &mut pending,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
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
        let mut parsed = parsed_with_comment(&format!(r#"env("{MISSING_ENV}")"#));
        resolve_resource_refs_defer_unknown_functions(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("permissive pass");

        let error = resolve_resource_refs_with_config(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect_err("strict pass must reject an unavailable unwrapped builtin");
        assert!(
            error
                .to_string()
                .contains(&format!("environment variable '{MISSING_ENV}' is not set")),
            "strict pass lost the original builtin diagnostic: {error}",
        );
    }

    #[test]
    fn secret_origin_4275_round3_strict_resolution_rejects_recorded_static_failure() {
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut parsed =
            parsed_with_comment(r#"secret(cidr_subnet("private-invalid-cidr", 1, 1))"#);
        resolve_resource_refs_defer_unknown_functions(&mut parsed, &config)
            .expect("permissive pass retains the protected failure");
        let error = resolve_resource_refs_with_config(&mut parsed, &config)
            .expect_err("a deterministic builtin failure must fail the strict pass");
        assert!(
            error
                .to_string()
                .contains(crate::resource::INSIDE_SECRET_MESSAGE),
            "{error}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(!rendered.contains("private-invalid-cidr"), "{rendered}");
        }
    }

    #[test]
    fn secret_wraps_an_unavailable_value_without_losing_taint() {
        let mut parsed = parsed_with_comment(&format!(r#"secret(env("{MISSING_ENV}"))"#));
        resolve_resource_refs_defer_unknown_functions(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("permissive pass");

        assert!(matches!(
            comment(&parsed),
            Value::Deferred(DeferredValue::Secret(inner))
                if matches!(inner.expose_plaintext(), Value::Deferred(DeferredValue::Unavailable {
                    name: crate::builtins::BuiltinFunction::env,
                    ..
                }))
        ));
    }

    #[test]
    fn unavailable_reason_does_not_expose_an_env_derived_variable_name() {
        const NAME_SOURCE: &str = "CARINA_TEST_UNAVAILABLE_NAME_SOURCE_3974";
        const SENSITIVE_NAME: &str = "CARINA_TEST_SENSITIVE_VARIABLE_NAME_3974";
        let config = ProviderContext::new(crate::EnvironmentSnapshot::new([(
            NAME_SOURCE.to_string(),
            SENSITIVE_NAME.into(),
        )]));
        let mut parsed =
            parsed_with_comment_and_config(&format!(r#"env(env("{NAME_SOURCE}"))"#), &config);
        resolve_resource_refs_defer_unknown_functions(&mut parsed, &config)
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
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
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
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("parse deferred for fixture");
        let crate::parser::DeferredForBody::Resource(template) =
            parsed.parsed_deferred_for_expressions()[0].body()
        else {
            panic!("deferred fixture body must be a resource")
        };
        assert!(matches!(
            template.attributes().get("comment"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));

        resolve_resource_refs_with_config(
            &mut parsed,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("resolve deferred template functions");

        let crate::parser::DeferredForBody::Resource(template) =
            parsed.parsed_deferred_for_expressions()[0].body()
        else {
            panic!("deferred fixture body must be a resource")
        };
        assert!(matches!(
            template.attributes().get("comment"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
    }
}

#[cfg(test)]
mod review2_cycle_tests {
    use super::*;
    #[test]
    fn review2_f8_let_cycle_keeps_public_chain_and_protects_secret_roots() {
        let reference = |name: &str| {
            Value::Deferred(DeferredValue::BindingRef {
                binding: name.into(),
            })
        };
        let public = IndexMap::from([("a".into(), reference("b")), ("b".into(), reference("a"))]);
        assert_eq!(
            reject_cyclic_let_bindings_in_variables(&public, &HashSet::new())
                .unwrap_err()
                .to_string(),
            "Cyclic let binding detected: a -> b -> a"
        );
        let private = IndexMap::from([
            (
                "public".into(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(reference("private_root")).into(),
                )),
            ),
            ("private_root".into(), reference("public")),
        ]);
        let error = reject_cyclic_let_bindings_in_variables(&private, &HashSet::new()).unwrap_err();
        assert!(
            !format!("{error} {error:?}").contains("private_root"),
            "{error:?}"
        );
    }
}
