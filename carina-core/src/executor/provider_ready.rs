//! Checked provider-boundary preparation.
//!
//! [`ProviderReady`] is the capability required by provider requests. Its
//! payload is private and this module is the only place that constructs it,
//! so normalization or identity resolution alone cannot cross the provider
//! boundary without value-constraint validation.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::sync::Mutex;

use indexmap::IndexMap;
use thiserror::Error;

use crate::binding_index::ResolvedBindings;
use crate::executor::normalized::apply_desired_normalization;
pub use crate::module_resolver::ModuleConstraintFailure;
use crate::module_resolver::{
    evaluate_live_pending_constraints, evaluate_saved_pending_constraints,
};
use crate::parser::{ProviderConfig, ResolvedProviderConfig};
use crate::provider::{
    CreateRequest, ProviderDirectives, ProviderFactory, ProviderNormalizer, UpdateRequest,
};
#[cfg(test)]
use crate::resource::NoPlaceholder;
use crate::resource::{
    BindingAddress, ConcreteValue, DataSource, DeferredValue, LiveComposition, ParserPlaceholder,
    PlaceholderKind, ProviderDataSource, ProviderModuleSource, ProviderResource,
    RelativeBindingName, ResolvedDataSource, ResolvedResource, Resource, ResourceId,
    SavedComposition, Value,
};
use crate::schema::{
    CompiledResourceSchema as ResourceSchema, SchemaKind, SchemaRegistry, TypeError,
};
use crate::value::SerializationError;

/// An immutable value that passed the provider-boundary constraint gate.
///
/// The fields are deliberately private. There is no `new`, `From`,
/// `Default`, or deserialization path; callers obtain this capability only
/// from [`prepare_provider_ready_resource`].
///
/// ```compile_fail
/// use carina_core::provider::ProviderReady;
/// use carina_core::resource::{ResolvedResource, Resource};
///
/// let resolved = ResolvedResource::new(Resource::new("test", "example"));
/// let _ready = ProviderReady { value: resolved }; // private fields
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderReady<T> {
    value: T,
    directives: ProviderDirectives,
}

impl<T> ProviderReady<T> {
    fn settled(
        pending: crate::validation::deferred_types::PendingProviderTypeChecks<
            '_,
            (T, ProviderDirectives),
        >,
    ) -> Result<Self, ProviderPreparationError> {
        let mut diagnostics = Vec::new();
        let (value, directives) = pending.publish(&mut diagnostics)?;
        if !diagnostics.is_empty() {
            let messages = diagnostics.into_iter().map(|diagnostic| match diagnostic {
                crate::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(warning)
                | crate::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(warning) => warning.message,
            }).collect::<Vec<_>>();
            return Err(ProviderPreparationError::Provider(
                crate::provider::ProviderError::invalid_input(messages.join("\n")),
            ));
        }
        Ok(Self { value, directives })
    }

    /// Borrow the checked payload.
    pub fn as_inner(&self) -> &T {
        &self.value
    }

    /// Borrow the schema-resolved directives paired with this payload.
    pub fn directives(&self) -> &ProviderDirectives {
        &self.directives
    }
}

impl<T> Deref for ProviderReady<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.as_inner()
    }
}

/// A fully resolved managed resource whose known value constraints have run.
pub type ProviderReadyResource = ProviderReady<ResolvedResource<ProviderModuleSource>>;

/// A fully resolved data source whose known input constraints have run.
pub type ProviderReadyDataSource = ProviderReady<ResolvedDataSource<ProviderModuleSource>>;

/// Provider configuration attributes whose schema-known enum leaves are in
/// host-resolved API spelling.
///
/// The field is private and the only producer is
/// [`validate_and_canonicalize_provider_config`], which also runs host type
/// checks and provider semantic validation before returning the value.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalProviderConfigAttributes {
    attributes: IndexMap<String, Value>,
}

impl CanonicalProviderConfigAttributes {
    pub fn attributes(&self) -> &IndexMap<String, Value> {
        &self.attributes
    }
}

impl Deref for CanonicalProviderConfigAttributes {
    type Target = IndexMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        self.attributes()
    }
}

/// Provider configuration that passed every provider-boundary check.
///
/// The attributes remain private so a provider instance cannot be created
/// from unchecked values. There is no `new`, `From`, `Default`, or
/// deserialization path; [`prepare_provider_ready_config`] is the sole
/// constructor.
///
/// ```compile_fail
/// use carina_core::provider::ProviderReadyConfig;
/// use indexmap::IndexMap;
///
/// let _ready = ProviderReadyConfig { attributes: IndexMap::new() };
/// ```
///
/// A raw attribute map cannot cross the provider construction boundary.
///
/// ```compile_fail
/// use carina_core::provider::ProviderFactory;
/// use carina_core::resource::Value;
/// use indexmap::IndexMap;
///
/// fn bypass(factory: &dyn ProviderFactory, attributes: &IndexMap<String, Value>) {
///     let _ = factory.create_initialized_provider(None, attributes);
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderReadyConfig {
    attributes: CanonicalProviderConfigAttributes,
}

impl ProviderReadyConfig {
    /// Borrow the checked provider configuration attributes.
    pub fn attributes(&self) -> &IndexMap<String, Value> {
        self.attributes.attributes()
    }
}

/// Dependencies shared by every checked provider-boundary preparation.
///
/// Keeping these values together gives create, update, and data-source
/// preparation one coherent execution snapshot, rather than letting each
/// downstream helper accept independently supplied views.
pub struct ProviderPreparationContext<'a> {
    bindings: &'a ResolvedBindings,
    module_gate: &'a ModuleConstraintGate,
    provider_configs: &'a [ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &'a dyn ProviderNormalizer,
    custom_type_resolver:
        crate::provider::FactoryCustomTypeResolver<&'a [Box<dyn ProviderFactory>]>,
    type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: &'a SchemaRegistry,
}

impl<'a> ProviderPreparationContext<'a> {
    pub fn new(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        provider_configs: &'a [ProviderConfig<
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
        >],
        normalizer: &'a dyn ProviderNormalizer,
        factories: &'a [Box<dyn ProviderFactory>],
        schemas: &'a SchemaRegistry,
        type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        let custom_type_resolver = crate::provider::FactoryCustomTypeResolver::new(factories);
        Self {
            bindings,
            module_gate,
            provider_configs,
            normalizer,
            custom_type_resolver,
            schemas,
            type_checks,
        }
    }

    pub(super) fn type_checks(
        &self,
    ) -> &dyn crate::validation::deferred_types::ProviderTypeCheckContext {
        self.type_checks
    }

    pub(super) fn bindings(&self) -> &ResolvedBindings {
        self.bindings
    }

    pub(super) fn schemas(&self) -> &SchemaRegistry {
        self.schemas
    }

    fn custom_type_resolver(&self) -> &dyn crate::parser::CustomTypeResolver {
        &self.custom_type_resolver
    }
}

/// Whether a provider-bound value is still in its authored form or came from
/// the plan after normalization. The latter always carries the authored
/// snapshot; omitting it is not representable at the preparation seam.
pub(super) enum CheckInput<T> {
    Authored(T),
    PlanNormalized { resolved: T, authored: T },
}

/// A resolved provider payload paired with the only valid schema-check scope.
///
/// The resolver constructs this value while it resolves references, so the
/// validation attribute map and the set that became known at apply cannot be
/// supplied independently by a call site.
pub(super) struct ResolvedCheckInput<T> {
    value: T,
    check: ResourceValueCheck,
}

impl<T> ResolvedCheckInput<T> {
    pub(super) fn authored(value: T) -> Self {
        Self {
            value,
            check: ResourceValueCheck::All,
        }
    }

    pub(super) fn plan_normalized(
        value: T,
        attributes: HashMap<String, Value>,
        names: HashSet<String>,
    ) -> Self {
        Self {
            value,
            check: ResourceValueCheck::ApplyResolved { attributes, names },
        }
    }

    pub(super) fn value(&self) -> &T {
        &self.value
    }
}

/// Aggregate returned by the module constraint gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConstraintGateError {
    failures: Vec<ModuleConstraintFailure>,
}

impl ModuleConstraintGateError {
    pub fn failures(&self) -> &[ModuleConstraintFailure] {
        &self.failures
    }
}

impl fmt::Display for ModuleConstraintGateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.is_empty() {
            return f.write_str(
                "provider dispatch blocked by an already reported module constraint violation",
            );
        }
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ModuleConstraintGateError {}

/// Apply-time module constraint gate shared by one execution.
///
/// Every call resolves and evaluates every composition's pending constraints
/// from the bindings supplied to that call. There is deliberately no
/// successful result cache or value fingerprint: a later binding change must
/// always be observed. The small reporting set only prevents a terminal sweep
/// from emitting the exact same already-reported violation a second time; it
/// is never consulted to skip evaluation.
pub struct ModuleConstraintGate {
    compositions: ModuleConstraintCompositions,
    reported_failures: Mutex<HashSet<ModuleConstraintFailure>>,
}

enum ModuleConstraintCompositions {
    Live(Vec<LiveComposition>),
    Saved(Vec<SavedComposition>),
}

impl ModuleConstraintGate {
    pub fn from_live_compositions(compositions: &[LiveComposition]) -> Self {
        Self {
            compositions: ModuleConstraintCompositions::Live(compositions.to_vec()),
            reported_failures: Mutex::new(HashSet::new()),
        }
    }

    pub fn from_saved_compositions(compositions: &[SavedComposition]) -> Self {
        Self {
            compositions: ModuleConstraintCompositions::Saved(compositions.to_vec()),
            reported_failures: Mutex::new(HashSet::new()),
        }
    }

    fn evaluate(
        &self,
        bindings: &ResolvedBindings,
        terminal: bool,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Vec<ModuleConstraintFailure> {
        match &self.compositions {
            ModuleConstraintCompositions::Live(compositions) => {
                evaluate_live_pending_constraints(compositions, bindings, terminal, session)
            }
            ModuleConstraintCompositions::Saved(compositions) => {
                evaluate_saved_pending_constraints(compositions, bindings, terminal, session)
            }
        }
    }

    /// Re-evaluate every pending constraint whose inputs may now be known.
    ///
    /// This is deliberately global rather than scoped to the provider-bound
    /// resource's module instance. A value can cross a nested module boundary
    /// or leave a module through an attribute before reaching its provider
    /// consumer, so the consumer's `module_source` does not identify every
    /// constraint that governs its inputs.
    ///
    /// One ceiling remains: a cross-argument `require` over X and Y can still
    /// be pending when a consumer of X runs if Y is unknown. It is evaluated
    /// when Y becomes known at a later gate, or rejected by the terminal sweep.
    pub fn check(
        &self,
        bindings: &ResolvedBindings,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<(), ModuleConstraintGateError> {
        let failures = self.evaluate(bindings, false, session);
        if failures.is_empty() {
            return Ok(());
        }

        let mut reported = self
            .reported_failures
            .lock()
            .expect("module constraint reporting lock poisoned");
        let new_failures = failures
            .into_iter()
            .filter(|failure| reported.insert(failure.clone()))
            .collect();

        // An empty diagnostic still returns Err: reporting suppression must
        // never turn a known violation into permission to call a provider.
        Err(ModuleConstraintGateError {
            failures: new_failures,
        })
    }

    /// Evaluate every remaining constraint at the end of apply.
    ///
    /// When `report_unresolved` is true, a constraint whose arguments are
    /// still unresolved is an explicit failure. An execution that already has
    /// failed or skipped effects passes false because those effects may be the
    /// only reason an input was never published. Decidable violations are
    /// evaluated and reported in either mode.
    ///
    /// Failures already emitted by a consuming provider effect are
    /// re-evaluated but omitted from this returned diagnostic only when the
    /// complete rendered failure is unchanged.
    pub fn finish(
        &self,
        bindings: &ResolvedBindings,
        report_unresolved: bool,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<(), ModuleConstraintGateError> {
        let failures = self.evaluate(bindings, report_unresolved, session);
        self.report_new(failures)
    }

    fn report_new(
        &self,
        failures: Vec<ModuleConstraintFailure>,
    ) -> Result<(), ModuleConstraintGateError> {
        let mut reported = self
            .reported_failures
            .lock()
            .expect("module constraint reporting lock poisoned");
        let failures = failures
            .into_iter()
            .filter(|failure| reported.insert(failure.clone()))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ModuleConstraintGateError { failures })
        }
    }
}

impl ProviderReadyResource {
    /// Borrow the provider-facing resource payload.
    pub fn as_resource(&self) -> &ProviderResource {
        self.value.as_resource()
    }
}

impl ProviderReadyDataSource {
    /// Borrow the provider-facing data-source payload.
    pub fn as_data_source(&self) -> &ProviderDataSource {
        self.value.as_inner()
    }
}

/// Failure while preparing a provider-facing resource.
#[derive(Debug, Error)]
pub enum ProviderPreparationError {
    #[error(transparent)]
    TypeSettlement(#[from] crate::validation::deferred_types::ProviderTypeCheckFailure),
    /// An unresolved deferred value survived apply-time reference resolution.
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    /// A known value violates its resource schema or provider validator.
    #[error("{resource}: value constraint failed before provider dispatch: {errors}")]
    ValueConstraint {
        resource: ResourceId,
        errors: ProviderValueConstraintErrors,
    },
    /// A pending module argument constraint became decidable and failed.
    #[error(transparent)]
    ModuleConstraint(#[from] ModuleConstraintGateError),
    /// Authored lifecycle metadata could not be resolved against the managed
    /// schema before constructing the provider-ready resource.
    #[error(transparent)]
    ResolvedResource(#[from] crate::resource::ResolvedResourceError),
    /// The managed resource has no schema to classify write-only attributes.
    #[error("{resource}: no managed schema is registered")]
    MissingManagedSchema { resource: ResourceId },
    /// An ignored write-only value would have to be resent even though its
    /// current provider value is unobservable.
    #[error(transparent)]
    IgnoredWriteOnly(#[from] crate::provider::IgnoredWriteOnlyValueError),
    /// Provider-specific normalization failed before dispatch.
    #[error("{0}")]
    Provider(crate::provider::ProviderError),
}

impl From<crate::provider::ProviderError> for ProviderPreparationError {
    fn from(error: crate::provider::ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl From<crate::value::EnumCanonicalizationError> for ProviderPreparationError {
    fn from(error: crate::value::EnumCanonicalizationError) -> Self {
        Self::Provider(error.into())
    }
}

/// Provider-specific configuration validation failure.
///
/// This wrapper retains the provider-specific
/// [`ProviderFactory::validate_config`] diagnostic as typed data.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct ProviderConfigValidationError {
    error: crate::provider::ProviderError,
}

impl ProviderConfigValidationError {
    pub fn provider_error(&self) -> &crate::provider::ProviderError {
        &self.error
    }
}

/// Failure while preparing configuration for provider construction.
#[derive(Debug, Error)]
pub enum ProviderConfigPreparationError {
    /// A concrete attribute violates its host-side type schema.
    #[error("provider {provider}: {attribute}: {error}")]
    InvalidAttribute {
        provider: String,
        attribute: String,
        error: ProviderValueConstraintErrors,
    },
    /// The provider rejected cross-attribute or provider-specific semantics.
    #[error("provider {provider}: {error}")]
    ProviderValidation {
        provider: String,
        error: ProviderConfigValidationError,
    },
}

impl ProviderConfigPreparationError {
    /// Attribute responsible for a host-side type/canonicalization failure.
    pub fn attribute(&self) -> Option<&str> {
        match self {
            Self::InvalidAttribute { attribute, .. } => Some(attribute),
            Self::ProviderValidation { .. } => None,
        }
    }

    /// Typed provider failure when provider-specific validation was reached.
    pub fn provider_error(&self) -> Option<&crate::provider::ProviderError> {
        match self {
            Self::InvalidAttribute { .. } => None,
            Self::ProviderValidation { error, .. } => Some(error.provider_error()),
        }
    }
}

fn canonicalize_provider_config_attributes<P: PlaceholderKind, B: BindingAddress>(
    provider_name: &str,
    attributes: &IndexMap<String, Value<P, B>>,
    attr_types: &crate::schema::ValidatedConfigSchema,
) -> Result<IndexMap<String, Value<P, B>>, ProviderConfigPreparationError> {
    let attributes = crate::value::canonicalize_provider_config_attributes_with_types(
        provider_name,
        attributes,
        attr_types.parts(),
    )
    .map_err(|error| {
        let (provider, attribute, error) = error.into_parts();
        ProviderConfigPreparationError::InvalidAttribute {
            provider,
            attribute,
            error: vec![error].into(),
        }
    })?;
    Ok(attributes)
}

fn validate_canonical_provider_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: IndexMap<String, Value>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let canonical = CanonicalProviderConfigAttributes { attributes };
    factory.validate_config(&canonical).map_err(|error| {
        let error = if canonical.values().any(crate::value::contains_secret) {
            error.with_redacted_message("configuration containing a secret failed validation")
        } else {
            error
        };
        ProviderConfigPreparationError::ProviderValidation {
            provider: provider_name.to_string(),
            error: ProviderConfigValidationError { error },
        }
    })?;
    Ok(canonical)
}

/// Validate and canonicalize one provider configuration through the single
/// host-to-provider seam.
///
/// The complete authored map is first canonicalized with provider-config enum
/// namespace rules, then host type checks run over that canonical map.
/// Attributes containing unresolved references are withheld from the provider
/// validator (the construction-time call has no such values), while every
/// remaining schema-known enum reaches [`ProviderFactory::validate_config`] in
/// API spelling.
pub fn validate_and_canonicalize_provider_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &IndexMap<String, Value>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let config_schema = factory.provider_config_attribute_types();
    let canonical =
        canonicalize_provider_config_attributes(provider_name, attributes, &config_schema)?;
    validate_provider_config_attributes(factory, provider_name, &canonical, &config_schema)?;

    let serializable = canonical
        .into_iter()
        .filter_map(|(name, value)| {
            value
                .try_into_address_free_absolute()
                .map(|value| (name, value))
        })
        .collect();
    validate_canonical_provider_config(factory, provider_name, serializable)
}

pub fn validate_parser_provider_config(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &IndexMap<String, Value<ParserPlaceholder, RelativeBindingName>>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let config_schema = factory.provider_config_attribute_types();
    let canonical =
        canonicalize_provider_config_attributes(provider_name, attributes, &config_schema)?;
    validate_provider_config_attributes(factory, provider_name, &canonical, &config_schema)?;

    let serializable = canonical
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_expanded()?
                .try_into_address_free_absolute()
                .map(|value| (name.clone(), value))
        })
        .collect();
    validate_canonical_provider_config(factory, provider_name, serializable)
}

/// Prepare provider configuration for construction and normalization.
///
/// [`ResolvedProviderConfig`] carries the guarantee that every construction-
/// time attribute is fully resolved. This boundary therefore checks each
/// attribute against the provider's host-side schema, replaces enum spellings
/// with canonical API values, and only then runs the provider-specific
/// configuration validator. This is the sole constructor for
/// [`ProviderReadyConfig`].
///
/// A raw parser config cannot skip the provider-resolution capability.
///
/// ```compile_fail
/// use carina_core::executor::prepare_provider_ready_config;
/// use carina_core::parser::ProviderConfig;
/// use carina_core::provider::ProviderFactory;
///
/// fn bypass(factory: &dyn ProviderFactory, raw: &ProviderConfig) {
///     let _ = prepare_provider_ready_config(factory, raw);
/// }
/// ```
pub fn prepare_provider_ready_config(
    factory: &dyn ProviderFactory,
    config: &ResolvedProviderConfig,
) -> Result<ProviderReadyConfig, ProviderConfigPreparationError> {
    let provider_name = config.name();
    let attributes = config.attributes();
    let canonical_attributes =
        validate_and_canonicalize_provider_config(factory, provider_name, attributes)?;

    Ok(ProviderReadyConfig {
        attributes: canonical_attributes,
    })
}

/// Run the host-owned provider configuration type checks.
///
/// The static validation phase calls this directly because unresolved values
/// are intentionally allowed there. Provider construction calls it only after
/// proving the complete map resolved.
fn validate_provider_config_attributes<P: PlaceholderKind, B: BindingAddress>(
    factory: &dyn ProviderFactory,
    provider_name: &str,
    attributes: &IndexMap<String, Value<P, B>>,
    attr_types: &crate::schema::ValidatedConfigSchema,
) -> Result<(), ProviderConfigPreparationError> {
    let lookup = crate::parser::custom_type_lookup_for_factory(factory);
    for (attribute, value) in attributes {
        if let Some(attr_type) = attr_types.rootless_type(attribute) {
            attr_type
                .validate_with_lookup(value, &lookup)
                .map_err(|errors| ProviderConfigPreparationError::InvalidAttribute {
                    provider: provider_name.to_string(),
                    attribute: attribute.clone(),
                    error: errors.into(),
                })?;
        }
    }
    Ok(())
}

/// Typed collection of schema failures from one provider-boundary check.
///
/// `Display` intentionally retains the historical `; `-joined text. This is a
/// composite leaf: exposing the first item through `Error::source` would make
/// a chain renderer repeat it while still omitting the remaining items. All
/// structured errors remain available through [`Self::errors`].
#[derive(Debug)]
pub struct ProviderValueConstraintErrors(Vec<TypeError>);

impl ProviderValueConstraintErrors {
    pub fn errors(&self) -> &[TypeError] {
        &self.0
    }
}

impl From<Vec<TypeError>> for ProviderValueConstraintErrors {
    fn from(errors: Vec<TypeError>) -> Self {
        Self(errors)
    }
}

impl fmt::Display for ProviderValueConstraintErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            error.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderValueConstraintErrors {}

/// Validate and prepare a resource for provider dispatch.
///
/// This is the sole constructor for [`ProviderReadyResource`]. It unwraps
/// secrets for the provider, validates every newly known schema value
/// (including provider custom types), re-runs the complete desired-side
/// normalization pipeline, and finally proves no provider-ineligible deferred value remains.
pub async fn prepare_provider_ready_resource(
    resource: Resource,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    prepare_provider_ready_resource_with_value_check(
        ResolvedCheckInput::authored(resource),
        context,
    )
    .await
}

/// Apply-path preparation when a pre-resolution source snapshot identifies
/// exactly which attributes became known during this effect.
///
/// The [`ResolvedCheckInput`] retains authored values for every other
/// attribute and the resolver-derived set that became known during this
/// effect. This prevents provider normalization performed during planning
/// from being mistaken for user input while still rerunning cross-attribute
/// validators when at least one input became known now.
pub(super) async fn prepare_provider_ready_resource_after_resolution<S: Clone>(
    input: ResolvedCheckInput<Resource<S>>,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    prepare_provider_ready_resource_with_value_check(input, context).await
}

enum ResourceValueCheck {
    All,
    ApplyResolved {
        attributes: HashMap<String, Value>,
        names: HashSet<String>,
    },
}

async fn prepare_provider_ready_resource_with_value_check<S: Clone>(
    input: ResolvedCheckInput<Resource<S>>,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context
        .module_gate
        .check(context.bindings, context.type_checks)?;
    if let Some(schema) = context.schemas.get_for(resource) {
        match value_check {
            ResourceValueCheck::All => validate_known_provider_values(
                &resource.id,
                schema,
                &resource.resolved_attributes(),
                &resource.quoted_string_attrs,
                context.custom_type_resolver(),
            )?,
            ResourceValueCheck::ApplyResolved { attributes, names } => {
                validate_selected_provider_values(
                    &resource.id,
                    schema,
                    &attributes,
                    &names,
                    &resource.quoted_string_attrs,
                    context.custom_type_resolver(),
                )?;
            }
        }
    }
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    let normalized = apply_desired_normalization(
        value,
        context.provider_configs,
        context.normalizer,
        context.schemas,
    )
    .await?;
    let resource = normalized.into_resource();
    crate::resource::assert_resource_fully_resolved(&resource)?;
    let resource =
        ResolvedResource::resolve_with_schemas(resource.into_provider_resource(), context.schemas)?;
    let directives = context.schemas.resolve_directives(
        &resource.as_resource().id,
        SchemaKind::Resource,
        &resource.as_resource().directives,
    );
    ProviderReady::settled(context.type_checks.pending((resource, directives)))
}

/// Validate and prepare a data source for provider dispatch.
///
/// This authored-input entry point runs every value constraint before schema
/// canonicalization, matching refresh and plan-time behavior. Apply execution
/// of a plan-normalized data source uses the private resolver-produced companion
/// path below, which checks only attributes that became known at apply. Both
/// paths produce the same sealed [`ProviderReadyDataSource`] witness before the
/// provider can be called.
pub fn prepare_provider_ready_data_source(
    resource: DataSource,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    prepare_provider_ready_data_source_after_resolution(
        ResolvedCheckInput::authored(resource),
        context,
    )
}

/// Apply-path data-source preparation for a value whose resolver has retained
/// authored inputs and derived which attributes became known during apply.
/// Plan-normalized literal attributes are deliberately outside that check
/// scope because they already passed the plan-time authored-value gate.
pub(super) fn prepare_provider_ready_data_source_after_resolution<S>(
    input: ResolvedCheckInput<DataSource<S>>,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context
        .module_gate
        .check(context.bindings, context.type_checks)?;
    if let Some(schema) = context.schemas.get_for_data_source(resource) {
        match value_check {
            ResourceValueCheck::All => validate_known_provider_values(
                &resource.id,
                schema,
                &crate::resource::attrs_to_hashmap(&resource.attributes),
                &resource.quoted_string_attrs,
                context.custom_type_resolver(),
            )?,
            ResourceValueCheck::ApplyResolved { attributes, names } => {
                validate_selected_provider_values(
                    &resource.id,
                    schema,
                    &attributes,
                    &names,
                    &resource.quoted_string_attrs,
                    context.custom_type_resolver(),
                )?;
            }
        }
    }
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    crate::value::canonicalize_data_sources_with_schemas(
        std::slice::from_mut(&mut value),
        context.schemas,
    )?;
    for value in value.attributes.values() {
        crate::resource::assert_value_fully_resolved(value)?;
    }
    let value = ResolvedDataSource::new(value.into_provider_data_source());
    let directives = context.schemas.resolve_directives(
        &value.as_inner().id,
        SchemaKind::DataSource,
        &value.as_inner().directives,
    );
    ProviderReady::settled(context.type_checks.pending((value, directives)))
}

/// Prepare a checked create request through the provider-boundary gate.
pub async fn prepare_create_request(
    resource: Resource,
    context: &ProviderPreparationContext<'_>,
) -> Result<CreateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(resource, context).await?;
    Ok(CreateRequest::checked(resource))
}

/// Look up the schema required to construct an update request.
pub(super) fn managed_update_schema<'a, S>(
    schemas: &'a SchemaRegistry,
    resource: &Resource<S>,
) -> Result<&'a ResourceSchema, ProviderPreparationError> {
    schemas
        .get_for(resource)
        .map(|schema| schema.schema().as_schema())
        .ok_or_else(|| ProviderPreparationError::MissingManagedSchema {
            resource: resource.id.clone(),
        })
}

/// Prepare a checked update request through the provider-boundary gate.
pub async fn prepare_update_request(
    resource: Resource,
    from: crate::resource::State,
    changed_attributes: &[String],
    context: &ProviderPreparationContext<'_>,
) -> Result<UpdateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(resource, context).await?;
    let schema = managed_update_schema(context.schemas, resource.as_resource())?;
    Ok(UpdateRequest::checked(
        from,
        changed_attributes,
        &resource,
        schema,
    )?)
}

fn validate_known_provider_values(
    id: &ResourceId,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &HashMap<String, Value>,
    quoted_string_attrs: &HashSet<String>,
    resolver: &dyn crate::parser::CustomTypeResolver,
) -> Result<(), ProviderPreparationError> {
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    schema
        .validate_known_values_with_resolver(attributes, &is_string_literal, resolver)
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: id.clone(),
            errors: errors.into(),
        })
}

fn validate_selected_provider_values(
    id: &ResourceId,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &HashMap<String, Value>,
    selected: &HashSet<String>,
    quoted_string_attrs: &HashSet<String>,
    resolver: &dyn crate::parser::CustomTypeResolver,
) -> Result<(), ProviderPreparationError> {
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    schema
        .validate_selected_known_values_with_resolver(
            attributes,
            selected,
            &is_string_literal,
            resolver,
        )
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: id.clone(),
            errors: errors.into(),
        })
}

fn unwrap_secret(value: Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => unwrap_secret(*inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(unwrap_secret(*value)),
                default: Box::new(unwrap_secret(*default)),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = unwrap_secret(expression.lhs().clone());
            let rhs = unwrap_secret(expression.rhs().clone());
            let original = Value::Deferred(DeferredValue::IntegerArithmetic(expression.clone()));
            expression
                .evaluate_with_operands(lhs, rhs)
                .unwrap_or(original)
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items.into_iter().map(unwrap_secret).collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.into_iter()
                .map(|(key, value)| (key, unwrap_secret(value)))
                .collect(),
        )),
        other => other,
    }
}

#[cfg(test)]
mod provider_config_tests {
    use crate::schema::{PatternAttributeSchema, PatternAttributeType, PatternResourceSchema};

    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResolvedProviderConfigs;
    use crate::provider::{ProviderError, ProviderResult};
    use crate::resource::{ConcreteValue, ParsedIgnorePath, ParsedIgnorePathSegment};
    use crate::schema::{
        CompiledAttributeType as AttributeType, RootlessType, TypeIdentity, legacy_validator,
    };

    #[tokio::test]
    async fn prepare_create_request_resolves_authored_ignore_changes() {
        let mut resource = Resource::with_provider("test", "widget", "example", None)
            .with_attribute(
                "comment",
                Value::Concrete(ConcreteValue::String("managed-elsewhere".into())),
            );
        resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
            ParsedIgnorePathSegment::Attribute("comment".to_string()),
        ])];
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("widget")
                .attribute(
                    PatternAttributeSchema::optional("comment", PatternAttributeType::string())
                        .build(),
                )
                .default_prevent_destroy()
                .validate()
                .expect("valid test schema"),
        );
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::new(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );

                let request = prepare_create_request(resource, &context)
                    .await
                    .expect("provider preparation must use schema resolution");

                assert_eq!(request.resource().ignore_changes().iter().len(), 1);
                assert!(request.directives().prevent_destroy());
            },
        )
        .await
    }

    #[tokio::test]
    async fn prepare_update_request_resolves_schema_default_directives() {
        let resource = Resource::with_provider("test", "widget", "example", None);
        let from = crate::resource::State::existing(resource.id.clone(), HashMap::new());
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("widget")
                .default_prevent_destroy()
                .validate()
                .expect("valid test schema"),
        );
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::new(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );

                let request = prepare_update_request(resource, from, &[], &context)
                    .await
                    .expect("provider preparation must resolve directives against the schema");

                assert!(request.directives().prevent_destroy());
            },
        )
        .await
    }

    #[tokio::test]
    async fn prepare_update_request_rejects_a_missing_managed_schema() {
        let resource = Resource::with_provider("test", "widget", "example", None);
        let id = resource.id.clone();
        let from = crate::resource::State::existing(id.clone(), HashMap::new());
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::new(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &[],
                    &schemas,
                    type_checks,
                );

                let error = prepare_update_request(resource, from, &[], &context)
                    .await
                    .expect_err("updates without a managed schema must fail preparation");

                assert!(matches!(
                    &error,
                    ProviderPreparationError::MissingManagedSchema { resource } if resource == &id
                ));
                let rendered = error.to_string();
                assert!(rendered.contains(&id.to_string()), "{rendered}");
                assert!(
                    rendered.contains("no managed schema is registered"),
                    "{rendered}"
                );
            },
        )
        .await
    }

    struct EnumConfigFactory;

    impl ProviderFactory for EnumConfigFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "enum config test provider"
        }

        fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
            crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                "region".to_string(),
                PatternAttributeType::enum_(
                    crate::schema::TypeIdentity::new(Some("test"), Vec::<String>::new(), "Region"),
                    Some(vec!["ap-northeast-1".to_string()]),
                    vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                    None,
                    None,
                ),
            )]))
            .expect("test provider config schema must be valid")
        }

        fn validate_config(
            &self,
            attributes: &CanonicalProviderConfigAttributes,
        ) -> crate::provider::ProviderResult<()> {
            match attributes.get("region") {
                Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                    if value.api_value() == "ap-northeast-1" =>
                {
                    Ok(())
                }
                other => Err(ProviderError::invalid_input(format!(
                    "expected canonical region, got {other:?}"
                ))),
            }
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "ap-northeast-1".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &ProviderReadyConfig,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    struct ConfigFactory {
        validate_calls: AtomicUsize,
        plugin_error: Option<ConfigFailure>,
    }

    #[derive(Clone, Copy)]
    enum ConfigFailure {
        Invalid(&'static str),
        Timeout(&'static str),
    }

    fn admit_declaration(declaration: crate::schema::AttributeType) -> AttributeType {
        crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "value".to_string(),
            declaration,
        )]))
        .unwrap()
        .parts()["value"]
            .clone()
    }

    struct RoutingFactory {
        name: &'static str,
        declaration: crate::schema::AttributeType,
        sibling_declaration: Option<crate::schema::AttributeType>,
        accepted: Option<&'static str>,
        validate_calls: Arc<AtomicUsize>,
        seen_values: Option<Arc<Mutex<Vec<String>>>>,
    }

    impl ProviderFactory for RoutingFactory {
        fn name(&self) -> &str {
            self.name
        }

        fn display_name(&self) -> &str {
            self.name
        }

        fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
            crate::schema::ValidatedConfigSchema::default()
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn validate_custom_type(
            &self,
            _identity: &TypeIdentity,
            value: &crate::provider::CanonicalCustomTypeValue,
        ) -> ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(seen_values) = &self.seen_values {
                seen_values.lock().unwrap().push(value.to_string());
            }
            match self.accepted {
                Some(expected) if value.as_str() != expected => {
                    Err(ProviderError::invalid_input(format!(
                        "{} accepts `{expected}`, not `{}`",
                        self.name,
                        value.as_str()
                    )))
                }
                Some(_) | None => Ok(()),
            }
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &ProviderReadyConfig,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("routing tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            let mut schema = PatternResourceSchema::new(format!("test.{}", self.name)).attribute(
                PatternAttributeSchema::optional("value", self.declaration.clone()).build(),
            );
            if let Some(sibling) = &self.sibling_declaration {
                schema = schema.attribute(
                    PatternAttributeSchema::optional("sibling", sibling.clone()).build(),
                );
            }
            vec![
                schema
                    .validate()
                    .expect("routing test schema must validate"),
            ]
        }
    }

    fn test_schema_path_resolver(
        factories: &[Box<dyn ProviderFactory>],
    ) -> impl crate::parser::CustomTypeResolver + '_ {
        crate::provider::FactoryCustomTypeResolver::new(factories)
    }

    fn test_schema_registry(factories: &[Box<dyn ProviderFactory>]) -> SchemaRegistry {
        let mut registry = SchemaRegistry::new();
        for factory in factories {
            for schema in factory.schemas() {
                registry.insert(factory.name(), schema);
            }
        }
        registry
    }

    fn test_lookup_for_owner<'a>(
        resolver: &'a dyn crate::parser::CustomTypeResolver,
        registry: &'a SchemaRegistry,
        owner: &str,
    ) -> impl Fn(&AttributeType, &Value) -> Result<(), TypeError> + use<'a> {
        let resource_type = format!("test.{owner}");
        let schema = registry
            .get(owner, &resource_type, crate::schema::SchemaKind::Resource)
            .expect("routing fixture schema must be registered under its owner");
        crate::parser::custom_type_lookup_for_schema(resolver, schema)
    }

    fn test_custom_type_validator(
        factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> crate::parser::CustomTypeValidatorFn {
        let resolver = crate::provider::FactoryCustomTypeResolver::new(factories);
        Box::new(move |target, value| {
            crate::parser::CustomTypeResolver::validate(&resolver, target, value)
        })
    }

    #[test]
    fn same_identity_routes_each_value_to_its_owning_factory() {
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(Some(identity), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("owner-value"),
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("other-value"),
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let owner_lookup = test_lookup_for_owner(&resolver, &registry, "owner");
        let other_lookup = test_lookup_for_owner(&resolver, &registry, "other");

        owner_lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("owner-value".into())),
        )
        .expect("the owner must apply its own verdict");
        assert!(
            owner_lookup(
                &admit_declaration(declaration.clone()),
                &Value::Concrete(ConcreteValue::String("other-value".into())),
            )
            .is_err(),
            "the owner's verdict must not be replaced by the colliding factory"
        );
        other_lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("other-value".into())),
        )
        .expect("the other factory must apply its own verdict");
        assert!(
            other_lookup(
                &admit_declaration(declaration.clone()),
                &Value::Concrete(ConcreteValue::String("owner-value".into())),
            )
            .is_err(),
            "the other factory's verdict must stay owner-qualified"
        );

        assert_eq!(owner_calls.load(Ordering::SeqCst), 2);
        assert_eq!(other_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn resource_lookup_rejects_an_unknown_schema_owner() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
        let resolver = test_schema_path_resolver(&factories);
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "missing",
            PatternResourceSchema::new("test.missing")
                .attribute(PatternAttributeSchema::optional("value", declaration.clone()).build())
                .validate()
                .expect("missing-owner fixture schema must validate"),
        );
        let lookup = test_lookup_for_owner(&resolver, &registry, "missing");

        let error = lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("value".into())),
        )
        .expect_err("an owner absent from the factory index must not disable validation");

        let TypeError::CustomTypeValidation {
            error,
            attribute: None,
        } = error
        else {
            panic!("expected typed custom-type validation error");
        };
        assert!(matches!(
            error.as_ref(),
            crate::parser::CustomTypeValidationError::MissingDeclaration { owner, identity: actual }
                if owner == "missing" && actual == &identity
        ));
    }

    #[test]
    fn unloaded_provider_context_never_approves_an_identified_declaration() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let context = crate::parser::ProviderContext::default();
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "missing",
            PatternResourceSchema::new("test.missing")
                .attribute(PatternAttributeSchema::optional("value", declaration.clone()).build())
                .validate()
                .expect("unloaded-owner fixture schema must validate"),
        );
        let lookup = test_lookup_for_owner(context.custom_type_resolver(), &registry, "missing");

        let error = lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("value".into())),
        )
        .expect_err("an unloaded context must not turn a custom declaration into success");

        assert!(matches!(
            error,
            TypeError::CustomTypeValidation { error, attribute: None }
                if matches!(error.as_ref(),
                    crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
        ));
    }

    #[test]
    fn shared_factory_resolver_rejects_an_unknown_schema_owner() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = AttributeType::refined_string(Some(identity.clone()), None, None, None);
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration)
            .expect("fixture declaration is identified");
        let factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
        let validator = crate::provider::FactoryCustomTypeResolver::new(factories.as_slice());

        let error = crate::parser::CustomTypeResolver::validate(
            &validator,
            crate::parser::CustomTypeValidationTarget::Declaration {
                owner: "missing",
                declaration,
            },
            &Value::Concrete(ConcreteValue::String("value".into())),
        )
        .expect_err("an owner absent from the shared index must not disable validation");

        assert!(matches!(
            error,
            crate::parser::CustomTypeValidationError::MissingDeclaration {
                owner,
                identity: actual,
            } if owner == "missing" && actual == identity
        ));
    }

    #[test]
    fn multi_owner_annotation_requires_every_owner_to_accept() {
        use crate::parser::{NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner};

        let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> = std::sync::Arc::new(vec![
            Box::new(RoutingFactory {
                name: "aws",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("arn:accepted"),
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "awscc",
                declaration,
                sibling_declaration: None,
                accepted: Some("arn:other"),
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
        ]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::with_owners(
            identity,
            NonEmptySchemaTypeOwners::try_from_iter([
                SchemaTypeOwner::Provider("aws".to_string()),
                SchemaTypeOwner::Provider("awscc".to_string()),
            ])
            .expect("two owners are non-empty"),
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::String("arn:accepted".into()));

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Annotation(&owned),
            &value,
        )
        .expect_err("one rejecting owner rejects the shared annotation");

        assert!(error.to_string().contains("awscc"), "{error}");
    }

    #[test]
    fn multi_owner_enum_definition_disagreement_is_typed() {
        use crate::parser::{
            CustomTypeValidationError, NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner,
        };

        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let first = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let second = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string(), "OTHER".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> = std::sync::Arc::new(vec![
            Box::new(RoutingFactory {
                name: "one",
                declaration: first,
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "two",
                declaration: second,
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
        ]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::with_owners(
            identity.clone(),
            NonEmptySchemaTypeOwners::try_from_iter([
                SchemaTypeOwner::Provider("two".to_string()),
                SchemaTypeOwner::Provider("one".to_string()),
            ])
            .expect("two owners are non-empty"),
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.COMMON"));

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Annotation(&owned),
            &value,
        )
        .expect_err("owners with different enum sets must not be first-matched");

        assert!(
            matches!(
                &error,
                CustomTypeValidationError::ConflictingEnumDefinitions {
                    identity: actual,
                    first_owner,
                    second_owner,
                } if actual == &identity && first_owner == "one" && second_owner == "two"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn sibling_enum_declarations_do_not_report_an_owner_conflict_with_itself() {
        use crate::parser::{CustomTypeValidationTarget, OwnedTypeIdentity};

        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let first = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let sibling = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string(), "OTHER".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration: first,
                sibling_declaration: Some(sibling),
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            })]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::provider(
            "owner",
            identity,
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.COMMON"));

        validator(CustomTypeValidationTarget::Annotation(&owned), &value)
            .expect("sibling declarations belong to one owner, not two conflicting owners");
    }

    #[test]
    fn declaration_local_schema_failure_remains_typed() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(
            Some(identity.clone()),
            Some("^accepted$".to_string()),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            })]);
        let validator = test_custom_type_validator(factories);
        let declaration = admit_declaration(declaration);
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration)
            .expect("fixture declaration is identified");

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Declaration {
                owner: "owner",
                declaration,
            },
            &Value::Concrete(ConcreteValue::String("rejected".into())),
        )
        .expect_err("the declaration pattern must reject the value");

        assert!(matches!(
            error,
            crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
                owner,
                identity: actual,
                error,
            } if owner == "owner"
                && actual == identity
                && matches!(error.as_ref(), TypeError::PatternMismatch { .. })
        ));
    }

    #[test]
    fn resource_lookup_validates_only_the_exact_declared_type() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let own_declaration = PatternAttributeType::refined_string(
            Some(identity.clone()),
            Some("^own$".to_string()),
            None,
            None,
        );
        let sibling_declaration = PatternAttributeType::refined_string(
            Some(identity),
            Some("^sibling$".to_string()),
            None,
            None,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: own_declaration.clone(),
            sibling_declaration: Some(sibling_declaration),
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        })];

        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        test_lookup_for_owner(&resolver, &registry, "owner")(
            &admit_declaration(own_declaration.clone()),
            &Value::Concrete(ConcreteValue::String("own".into())),
        )
        .expect("a sibling declaration with the same identity must not constrain this attribute");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn same_identity_enum_spelling_resolves_in_the_owning_schema() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let owner_type = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["OWNER_API".to_string()]),
            vec![("OWNER_API".to_string(), "owner".to_string())],
            None,
            None,
        );
        let other_type = PatternAttributeType::enum_(
            identity,
            Some(vec!["OTHER_API".to_string()]),
            vec![("OTHER_API".to_string(), "other".to_string())],
            None,
            None,
        );
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: owner_type.clone(),
                sibling_declaration: None,
                accepted: Some("OWNER_API"),
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: other_type.clone(),
                sibling_declaration: None,
                accepted: Some("OTHER_API"),
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];

        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        test_lookup_for_owner(&resolver, &registry, "owner")(
            &admit_declaration(owner_type.clone()),
            &Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.owner")),
        )
        .expect("the owner alias must resolve to the owner's API spelling");
        test_lookup_for_owner(&resolver, &registry, "other")(
            &admit_declaration(other_type.clone()),
            &Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.other")),
        )
        .expect("the other alias must resolve to the other API spelling");

        assert_eq!(owner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn owning_factory_schema_rejects_scalar_shape_mismatch() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Count");
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = RoutingFactory {
            name: "owner",
            declaration: PatternAttributeType::refined_int(Some(identity.clone()), None),
            sibling_declaration: None,
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        };

        let error = crate::provider::validate_custom_type_with_factory(
            &factory,
            crate::parser::IdentifiedCustomTypeDeclaration::new(&admit_declaration(
                factory.declaration.clone(),
            ))
            .expect("fixture declaration is identified"),
            &Value::Concrete(ConcreteValue::String("5".into())),
        )
        .expect_err("an Int declaration must not accept bridge-shaped String input");

        let rendered = error.to_string();
        assert!(rendered.contains("expected shared.Count"), "{rendered}");
        assert!(rendered.contains("got String"), "{rendered}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn executor_lookup_receives_canonical_numeric_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen_values = Arc::new(Mutex::new(Vec::new()));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Number");
        let declaration = PatternAttributeType::refined_float(Some(identity), None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: declaration.clone(),
            sibling_declaration: None,
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: Some(Arc::clone(&seen_values)),
        })];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        for value in [
            Value::Concrete(ConcreteValue::Int(60)),
            Value::Concrete(ConcreteValue::Int(-61)),
            Value::Concrete(ConcreteValue::Float(1.25)),
        ] {
            lookup(&admit_declaration(declaration.clone()), &value)
                .expect("numeric scalar must bridge");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(*seen_values.lock().unwrap(), ["60", "-61", "1.25"]);
    }

    #[test]
    fn decoded_refinement_bridge_calls_only_the_owning_factory_once() {
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(Some(identity), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let decoded = crate::protocol_convert::protocol_to_attribute_type(
            &carina_provider_protocol::types::AttributeType::String {
                pattern: None,
                length: None,
                validate: None,
                to_dsl: None,
                identity: Some("shared.Identifier".to_string()),
            },
        )
        .expect("callback-free protocol refinement must decode");
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "decoded".to_string(),
            decoded,
        )]))
        .unwrap()
        .rootless_type("decoded")
        .unwrap()
        .validate_with_lookup(
            &Value::<NoPlaceholder>::Concrete(ConcreteValue::String("value".into())),
            &lookup,
        )
        .expect("the owning bridge validator must accept the value");

        assert_eq!(owner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_refinement_callback_runs_once_without_factory_fallback() {
        let native_calls = Arc::new(AtomicUsize::new(0));
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let calls = Arc::clone(&native_calls);
        let native_type = PatternAttributeType::refined_string_with_validator(
            identity.clone(),
            None,
            None,
            legacy_validator(move |_value| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        );
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: PatternAttributeType::refined_string(
                    Some(identity.clone()),
                    None,
                    None,
                    None,
                ),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: PatternAttributeType::refined_string(Some(identity), None, None, None),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        RootlessType::new(native_type)
            .validate_with_lookup(
                &Value::<NoPlaceholder>::Concrete(ConcreteValue::String("value".into())),
                &lookup,
            )
            .expect("the native callback must accept the value");

        assert_eq!(native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(owner_calls.load(Ordering::SeqCst), 0);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    struct RefinedConfigFactory {
        validate_calls: AtomicUsize,
    }

    impl ProviderFactory for RefinedConfigFactory {
        fn name(&self) -> &str {
            "owner"
        }

        fn display_name(&self) -> &str {
            "refined config test provider"
        }

        fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
            crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                "endpoint".to_string(),
                PatternAttributeType::refined_string(
                    Some(TypeIdentity::new(
                        Some("shared"),
                        Vec::<String>::new(),
                        "Endpoint",
                    )),
                    None,
                    None,
                    None,
                ),
            )]))
            .expect("test provider config schema must be valid")
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn validate_custom_type(
            &self,
            _identity: &TypeIdentity,
            value: &crate::provider::CanonicalCustomTypeValue,
        ) -> ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            if value.as_str() == "valid" {
                Ok(())
            } else {
                Err(ProviderError::invalid_input("invalid endpoint"))
            }
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &ProviderReadyConfig,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    #[test]
    fn decoded_refined_provider_config_calls_its_factory_once() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let attributes = IndexMap::from([(
            "endpoint".to_string(),
            Value::Concrete(ConcreteValue::String("valid".into())),
        )]);

        validate_and_canonicalize_provider_config(&factory, "owner", &attributes)
            .expect("the owning factory must validate its decoded refined config type");

        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_config_lookup_routes_through_selected_factory_not_loose_name() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let attributes = IndexMap::from([(
            "endpoint".to_string(),
            Value::Concrete(ConcreteValue::String("valid".into())),
        )]);

        validate_and_canonicalize_provider_config(&factory, "display-alias", &attributes)
            .expect("the already-selected factory is the provider-config routing authority");

        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    impl ConfigFactory {
        fn successful() -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: None,
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: Some(ConfigFailure::Invalid(message)),
            }
        }

        fn timing_out(message: &'static str) -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: Some(ConfigFailure::Timeout(message)),
            }
        }
    }

    impl ProviderFactory for ConfigFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "provider config preparation test provider"
        }

        fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
            crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                "region".to_string(),
                PatternAttributeType::refined_string(
                    None,
                    Some(r"^good-[a-z]+$".to_string()),
                    None,
                    None,
                ),
            )]))
            .expect("test provider config schema must be valid")
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
        ) -> crate::provider::ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            match self.plugin_error {
                Some(ConfigFailure::Invalid(message)) => Err(ProviderError::invalid_input(message)),
                Some(ConfigFailure::Timeout(message)) => Err(ProviderError::timeout(message)
                    .with_host_cause(crate::provider::HostErrorCause::ValidationSuspended(
                        crate::provider::SuspendedValidationId::from_keyed_fingerprint([0_u8; 32]),
                    ))),
                None => Ok(()),
            }
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &ProviderReadyConfig,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    fn config(value: Value) -> IndexMap<String, Value> {
        IndexMap::from([("region".to_string(), value)])
    }

    fn resolved_configs(attributes: IndexMap<String, Value>) -> ResolvedProviderConfigs {
        ResolvedProviderConfigs::from_test_configs(vec![crate::parser::ProviderConfig {
            name: "test".to_string(),
            attributes,
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }])
        .expect("test provider attributes must satisfy the resolved-config invariant")
    }

    #[test]
    fn prepare_provider_ready_config_accepts_valid_attributes() {
        let factory = ConfigFactory::successful();
        let attributes = config(Value::Concrete(ConcreteValue::String("good-tokyo".into())));
        let resolved = resolved_configs(attributes.clone());

        let ready = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect("valid provider config must be prepared");

        assert_eq!(ready.attributes(), &attributes);
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parser_provider_config_skips_unexpandable_placeholder_without_panicking() {
        let factory = ConfigFactory::successful();
        let attributes = IndexMap::from([(
            "region".to_string(),
            Value::Deferred(DeferredValue::Placeholder(
                ParserPlaceholder::EmptyInterpolation {
                    source: crate::resource::DiagnosticSourceSpan::authored(
                        "test.crn".into(),
                        1,
                        1,
                        1,
                        4,
                    ),
                },
            )),
        )]);

        let canonical = validate_parser_provider_config(&factory, "test", &attributes)
            .expect("placeholder leaves are unknown during parser-stage validation");

        assert!(canonical.attributes().is_empty());
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_provider_ready_config_canonicalizes_enum_before_plugin_validation() {
        let attributes = config(Value::Concrete(ConcreteValue::enum_identifier(
            "test.Region.ap_northeast_1",
        )));
        let resolved = resolved_configs(attributes);

        let ready =
            prepare_provider_ready_config(&EnumConfigFactory, resolved.iter().next().unwrap())
                .expect("provider config enum must be canonicalized before validation");

        assert!(matches!(
            ready.attributes().get("region"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "ap-northeast-1"
        ));
    }

    #[test]
    fn prepare_provider_ready_config_rejects_pattern_violation() {
        let factory = ConfigFactory::successful();
        let attributes = config(Value::Concrete(ConcreteValue::String("bad".into())));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("pattern mismatch must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(message.contains("region"), "{message}");
        assert!(message.contains(r"^good-[a-z]+$"), "{message}");
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(message.matches(r"^good-[a-z]+$").count(), 1, "{message}");
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn prepare_provider_ready_config_rejects_plugin_validation_failure() {
        let factory = ConfigFactory::failing("configured account is forbidden");
        let attributes = config(Value::Concrete(ConcreteValue::String("good-tokyo".into())));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(
            message.contains("configured account is forbidden"),
            "{message}"
        );
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(
            message.matches("configured account is forbidden").count(),
            1,
            "{message}"
        );
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_provider_ready_config_masks_plugin_errors_for_secrets() {
        let plaintext = "plaintext-must-not-leak";
        let factory = ConfigFactory::failing(plaintext);
        let attributes = config(Value::Deferred(DeferredValue::Secret(Box::new(
            Value::Concrete(ConcreteValue::String("good-secret".into())),
        ))));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(message.contains("secret"), "{message}");
        assert!(!message.contains(plaintext), "{message}");
    }

    #[test]
    fn secret_error_redaction_preserves_provider_error_kind() {
        let plaintext = "plaintext-timeout-must-not-leak";
        let factory = ConfigFactory::timing_out(plaintext);
        let attributes = config(Value::Deferred(DeferredValue::Secret(Box::new(
            Value::Concrete(ConcreteValue::String("good-secret".into())),
        ))));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let provider_error = error
            .provider_error()
            .expect("provider validation errors retain their typed error");

        assert_eq!(
            provider_error.kind(),
            crate::provider::ProviderErrorKind::Timeout
        );
        assert!(matches!(
            provider_error.detail().host_cause(),
            Some(crate::provider::HostErrorCause::ValidationSuspended(_))
        ));
        assert!(!error.to_string().contains(plaintext));
    }

    #[test]
    fn provider_boundary_unwraps_secret_builtin_results_to_plaintext() {
        let value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("prefix-decrypted:ciphertext-suffix".into()),
        ))));

        assert_eq!(
            unwrap_secret(value),
            Value::Concrete(ConcreteValue::String(
                "prefix-decrypted:ciphertext-suffix".into()
            ))
        );
    }

    #[test]
    fn provider_boundary_unwraps_secrets_in_both_coalesce_operands() {
        let secret = |value: &str| {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(value.into()),
            ))))
        };
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(secret("left")),
            default: Box::new(secret("right")),
        });

        assert_eq!(
            unwrap_secret(value),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Concrete(ConcreteValue::String("left".into()))),
                default: Box::new(Value::Concrete(ConcreteValue::String("right".into()))),
            })
        );
    }
}
