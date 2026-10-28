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
use crate::module_resolver::evaluate_pending_constraints;
use crate::parser::{ProviderConfig, ResolvedProviderConfig};
use crate::provider::{
    CanonicalCustomTypeValue, CreateRequest, ProviderError, ProviderFactory, ProviderNormalizer,
    ProviderResult, UpdateRequest,
};
use crate::resource::{
    Composition, ConcreteValue, DataSource, DeferredValue, ResolvedDataSource, ResolvedResource,
    Resource, ResourceId, Value,
};
use crate::schema::{ResourceSchema, SchemaRegistry, TypeError, TypeIdentity};
use crate::value::SerializationError;

/// An immutable value that passed the provider-boundary constraint gate.
///
/// The inner field is deliberately private. There is no `new`, `From`,
/// `Default`, or deserialization path; callers obtain this capability only
/// from [`prepare_provider_ready_resource`].
///
/// ```compile_fail
/// use carina_core::provider::ProviderReady;
/// use carina_core::resource::{ResolvedResource, Resource};
///
/// let resolved = ResolvedResource::new(Resource::new("test", "example"));
/// let _ready = ProviderReady(resolved); // private tuple field
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderReady<T>(T);

impl<T> ProviderReady<T> {
    /// Borrow the checked payload.
    pub fn as_inner(&self) -> &T {
        &self.0
    }
}

impl<T> Deref for ProviderReady<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.as_inner()
    }
}

/// A fully resolved managed resource whose known value constraints have run.
pub type ProviderReadyResource = ProviderReady<ResolvedResource>;

/// A fully resolved data source whose known input constraints have run.
pub type ProviderReadyDataSource = ProviderReady<ResolvedDataSource>;

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
///     let _ = factory.create_provider(None, attributes);
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
#[derive(Clone, Copy)]
pub struct ProviderPreparationContext<'a> {
    bindings: &'a ResolvedBindings,
    module_gate: &'a ModuleConstraintGate,
    provider_configs: &'a [ProviderConfig],
    normalizer: &'a dyn ProviderNormalizer,
    factories: &'a [Box<dyn ProviderFactory>],
    schemas: &'a SchemaRegistry,
}

impl<'a> ProviderPreparationContext<'a> {
    pub fn new(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        provider_configs: &'a [ProviderConfig],
        normalizer: &'a dyn ProviderNormalizer,
        factories: &'a [Box<dyn ProviderFactory>],
        schemas: &'a SchemaRegistry,
    ) -> Self {
        Self {
            bindings,
            module_gate,
            provider_configs,
            normalizer,
            factories,
            schemas,
        }
    }

    pub(super) fn bindings(&self) -> &ResolvedBindings {
        self.bindings
    }

    pub(super) fn schemas(&self) -> &SchemaRegistry {
        self.schemas
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
    compositions: Vec<Composition>,
    reported_failures: Mutex<HashSet<ModuleConstraintFailure>>,
}

impl ModuleConstraintGate {
    pub fn new(compositions: &[Composition]) -> Self {
        Self {
            compositions: compositions.to_vec(),
            reported_failures: Mutex::new(HashSet::new()),
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
    pub fn check(&self, bindings: &ResolvedBindings) -> Result<(), ModuleConstraintGateError> {
        let failures = evaluate_pending_constraints(&self.compositions, bindings, false);
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
    ) -> Result<(), ModuleConstraintGateError> {
        let failures =
            evaluate_pending_constraints(&self.compositions, bindings, report_unresolved);
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
    pub fn as_resource(&self) -> &Resource {
        self.0.as_resource()
    }
}

impl ProviderReadyDataSource {
    /// Borrow the provider-facing data-source payload.
    pub fn as_data_source(&self) -> &DataSource {
        self.0.as_inner()
    }
}

/// Failure while preparing a provider-facing resource.
#[derive(Debug, Error)]
pub enum ProviderPreparationError {
    /// A deferred placeholder survived apply-time reference resolution.
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
    /// The managed resource has no schema to classify write-only attributes.
    #[error("{resource}: no managed schema is registered")]
    MissingManagedSchema { resource: ResourceId },
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
        error: Box<TypeError>,
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

fn canonicalize_provider_config_attributes(
    provider_name: &str,
    attributes: &IndexMap<String, Value>,
    attr_types: &HashMap<String, crate::schema::AttributeType>,
) -> Result<CanonicalProviderConfigAttributes, ProviderConfigPreparationError> {
    let attributes = crate::value::canonicalize_provider_config_attributes_with_types(
        provider_name,
        attributes,
        attr_types,
    )
    .map_err(|error| {
        let (provider, attribute, error) = error.into_parts();
        ProviderConfigPreparationError::InvalidAttribute {
            provider,
            attribute,
            error: Box::new(error),
        }
    })?;
    Ok(CanonicalProviderConfigAttributes { attributes })
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
    let attr_types = factory.provider_config_attribute_types();
    let canonical =
        canonicalize_provider_config_attributes(provider_name, attributes, &attr_types)?;
    validate_provider_config_attributes(provider_name, canonical.attributes(), &attr_types)?;

    let serializable = canonical
        .attributes
        .into_iter()
        .filter(|(_, value)| !crate::validation::value_contains_unresolved_ref(value))
        .collect();
    let canonical = CanonicalProviderConfigAttributes {
        attributes: serializable,
    };

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
fn validate_provider_config_attributes(
    provider_name: &str,
    attributes: &IndexMap<String, Value>,
    attr_types: &HashMap<String, crate::schema::AttributeType>,
) -> Result<(), ProviderConfigPreparationError> {
    let schema_view = crate::schema::Schema::with_defs(std::collections::BTreeMap::new());
    for (attribute, value) in attributes {
        if let Some(attr_type) = attr_types.get(attribute) {
            schema_view
                .validate_attr(attr_type, value)
                .map_err(|error| ProviderConfigPreparationError::InvalidAttribute {
                    provider: provider_name.to_string(),
                    attribute: attribute.clone(),
                    error: Box::new(error),
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
/// normalization pipeline, and finally proves no deferred placeholder remains.
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
pub(super) async fn prepare_provider_ready_resource_after_resolution(
    input: ResolvedCheckInput<Resource>,
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

async fn prepare_provider_ready_resource_with_value_check(
    input: ResolvedCheckInput<Resource>,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context.module_gate.check(context.bindings)?;
    if let Some(schema) = context.schemas.get_for(resource) {
        match value_check {
            ResourceValueCheck::All => validate_known_provider_values(
                &resource.id,
                schema,
                &resource.resolved_attributes(),
                &resource.quoted_string_attrs,
                context.factories,
            )?,
            ResourceValueCheck::ApplyResolved { attributes, names } => {
                validate_selected_provider_values(
                    &resource.id,
                    schema,
                    &attributes,
                    &names,
                    &resource.quoted_string_attrs,
                    context.factories,
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
    Ok(ProviderReady(ResolvedResource::new(resource)))
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
pub(super) fn prepare_provider_ready_data_source_after_resolution(
    input: ResolvedCheckInput<DataSource>,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context.module_gate.check(context.bindings)?;
    if let Some(schema) = context.schemas.get_for_data_source(resource) {
        match value_check {
            ResourceValueCheck::All => validate_known_provider_values(
                &resource.id,
                schema,
                &crate::resource::attrs_to_hashmap(&resource.attributes),
                &resource.quoted_string_attrs,
                context.factories,
            )?,
            ResourceValueCheck::ApplyResolved { attributes, names } => {
                validate_selected_provider_values(
                    &resource.id,
                    schema,
                    &attributes,
                    &names,
                    &resource.quoted_string_attrs,
                    context.factories,
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
    Ok(ProviderReady(ResolvedDataSource::new(value)))
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
pub(super) fn managed_update_schema<'a>(
    schemas: &'a SchemaRegistry,
    resource: &Resource,
) -> Result<&'a ResourceSchema, ProviderPreparationError> {
    schemas
        .get_for(resource)
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
    ))
}

/// Build a custom-type lookup that retains typed provider failures for callers
/// with retry or diagnostic policy.
pub fn provider_custom_type_lookup(
    factories: &[Box<dyn ProviderFactory>],
) -> impl Fn(&TypeIdentity, &Value) -> ProviderResult<()> + Send + Sync + '_ {
    move |identity, value| {
        let canonical = match value {
            Value::Concrete(ConcreteValue::String(text)) => {
                CanonicalCustomTypeValue::from_literal_string(text)
            }
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.identity().same_type(identity) =>
            {
                CanonicalCustomTypeValue::from_canonical_enum(value)
            }
            Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                return Err(ProviderError::invalid_input(format!(
                    "unresolved enum identifier `{raw}` reached provider custom-type validation"
                )));
            }
            _ => return Ok(()),
        };
        for factory in factories {
            factory
                .validate_custom_type(identity, &canonical)
                .map_err(|error| {
                    if error.detail().provider_name.is_some() {
                        error
                    } else {
                        error.for_provider(factory.name())
                    }
                })?;
        }
        Ok(())
    }
}

fn validate_known_provider_values(
    id: &ResourceId,
    schema: &ResourceSchema,
    attributes: &HashMap<String, Value>,
    quoted_string_attrs: &HashSet<String>,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), ProviderPreparationError> {
    let provider_lookup = provider_custom_type_lookup(factories);
    let lookup = |identity: &TypeIdentity, value: &Value| {
        provider_lookup(identity, value).map_err(|error| TypeError::ValidationFailed {
            message: error.to_string(),
        })
    };
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    schema
        .validate_known_values_with_origins_and_lookup(attributes, &is_string_literal, &lookup)
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: id.clone(),
            errors: errors.into(),
        })
}

fn validate_selected_provider_values(
    id: &ResourceId,
    schema: &ResourceSchema,
    attributes: &HashMap<String, Value>,
    selected: &HashSet<String>,
    quoted_string_attrs: &HashSet<String>,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), ProviderPreparationError> {
    let provider_lookup = provider_custom_type_lookup(factories);
    let lookup = |identity: &TypeIdentity, value: &Value| {
        provider_lookup(identity, value).map_err(|error| TypeError::ValidationFailed {
            message: error.to_string(),
        })
    };
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    schema
        .validate_selected_known_values_with_origins_and_lookup(
            attributes,
            selected,
            &is_string_literal,
            &lookup,
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
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::future::BoxFuture;
    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResolvedProviderConfigs;
    use crate::provider::{Provider, ProviderResult};
    use crate::resource::ConcreteValue;
    use crate::schema::AttributeType;

    #[tokio::test]
    async fn prepare_update_request_rejects_a_missing_managed_schema() {
        let resource = Resource::with_provider("test", "widget", "example", None);
        let id = resource.id.clone();
        let from = crate::resource::State::existing(id.clone(), HashMap::new());
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::new(&[]);
        let schemas = SchemaRegistry::new();
        let context = ProviderPreparationContext::new(
            &bindings,
            &module_gate,
            &[],
            &crate::provider::NoopNormalizer,
            &[],
            &schemas,
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
    }

    struct EnumConfigFactory;

    impl ProviderFactory for EnumConfigFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "enum config test provider"
        }

        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::from([(
                "region".to_string(),
                AttributeType::enum_(
                    crate::schema::TypeIdentity::new(Some("test"), Vec::<String>::new(), "Region"),
                    Some(vec!["ap-northeast-1".to_string()]),
                    vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                    None,
                    None,
                ),
            )])
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
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schemas(&self) -> Vec<ResourceSchema> {
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

        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::from([(
                "region".to_string(),
                AttributeType::refined_string(None, Some(r"^good-[a-z]+$".to_string()), None, None),
            )])
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
        ) -> crate::provider::ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            match self.plugin_error {
                Some(ConfigFailure::Invalid(message)) => Err(ProviderError::invalid_input(message)),
                Some(ConfigFailure::Timeout(message)) => Err(ProviderError::timeout(message)
                    .with_host_cause(crate::provider::HostErrorCause::ValidationSuspended)),
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
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schemas(&self) -> Vec<ResourceSchema> {
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
        assert_eq!(
            provider_error.detail().host_cause(),
            Some(crate::provider::HostErrorCause::ValidationSuspended)
        );
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
