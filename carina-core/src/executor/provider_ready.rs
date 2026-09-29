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

use thiserror::Error;

use crate::binding_index::ResolvedBindings;
use crate::executor::normalized::apply_desired_normalization;
pub use crate::module_resolver::ModuleConstraintFailure;
use crate::module_resolver::evaluate_pending_constraints;
use crate::parser::ProviderConfig;
use crate::provider::{CreateRequest, ProviderFactory, ProviderNormalizer, UpdateRequest};
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

    /// Borrow the identity-resolved resource wrapper.
    pub fn as_resolved_resource(&self) -> &ResolvedResource {
        &self.0
    }
}

impl ProviderReadyDataSource {
    /// Borrow the provider-facing data-source payload.
    pub fn as_data_source(&self) -> &DataSource {
        self.0.as_inner()
    }

    /// Borrow the identity-resolved data-source wrapper.
    pub fn as_resolved_data_source(&self) -> &ResolvedDataSource {
        &self.0
    }
}

/// Failure while preparing a provider-facing resource.
#[derive(Debug, Error)]
pub enum ProviderPreparationError {
    /// A deferred placeholder survived apply-time reference resolution.
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    /// A known value violates its resource schema or provider validator.
    #[error("{resource}: value constraint failed before provider dispatch: {message}")]
    ValueConstraint {
        resource: ResourceId,
        message: String,
    },
    /// A pending module argument constraint became decidable and failed.
    #[error(transparent)]
    ModuleConstraint(#[from] ModuleConstraintGateError),
}

/// Validate and prepare a resource for provider dispatch.
///
/// This is the sole constructor for [`ProviderReadyResource`]. It unwraps
/// secrets for the provider, validates every newly known schema value
/// (including provider custom types), re-runs the complete desired-side
/// normalization pipeline, and finally proves no deferred placeholder remains.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_provider_ready_resource(
    resource: Resource,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    prepare_provider_ready_resource_with_value_check(
        resource,
        bindings,
        module_gate,
        provider_configs,
        normalizer,
        factories,
        schemas,
        ResourceValueCheck::All,
    )
    .await
}

/// Apply-path preparation when a pre-resolution source snapshot identifies
/// exactly which attributes became known during this effect.
///
/// The complete `validation_attributes` map retains authored values for every
/// other attribute. This prevents provider normalization performed during
/// planning from being mistaken for user input while still rerunning
/// cross-attribute validators when at least one input became known now.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_provider_ready_resource_after_resolution(
    resource: Resource,
    validation_attributes: &HashMap<String, Value>,
    resolved_attributes: &HashSet<String>,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    prepare_provider_ready_resource_with_value_check(
        resource,
        bindings,
        module_gate,
        provider_configs,
        normalizer,
        factories,
        schemas,
        ResourceValueCheck::ApplyResolved {
            attributes: validation_attributes,
            names: resolved_attributes,
        },
    )
    .await
}

enum ResourceValueCheck<'a> {
    All,
    ApplyResolved {
        attributes: &'a HashMap<String, Value>,
        names: &'a HashSet<String>,
    },
}

#[allow(clippy::too_many_arguments)]
async fn prepare_provider_ready_resource_with_value_check(
    mut resource: Resource,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
    value_check: ResourceValueCheck<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    module_gate.check(bindings)?;
    if let Some(schema) = schemas.get_for(&resource) {
        match value_check {
            ResourceValueCheck::All => validate_known_provider_values(
                &resource.id,
                schema,
                &resource.resolved_attributes(),
                &resource.quoted_string_attrs,
                factories,
            )?,
            ResourceValueCheck::ApplyResolved { attributes, names } => {
                validate_selected_provider_values(
                    &resource.id,
                    schema,
                    attributes,
                    names,
                    &resource.quoted_string_attrs,
                    factories,
                )?;
            }
        }
    }
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    let normalized =
        apply_desired_normalization(resource, provider_configs, normalizer, factories, schemas)
            .await;
    let resource = normalized.into_resource();
    crate::resource::assert_resource_fully_resolved(&resource)?;
    Ok(ProviderReady(ResolvedResource::new(resource)))
}

/// Validate and prepare a data source for provider dispatch.
///
/// Value constraints intentionally run before schema canonicalization, matching
/// the plan-time pipeline. Once the authored values pass, the provider receives
/// the canonical representation.
/// This is the sole constructor for [`ProviderReadyDataSource`].
pub fn prepare_provider_ready_data_source(
    mut resource: DataSource,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    module_gate.check(bindings)?;
    if let Some(schema) = schemas.get_for_data_source(&resource) {
        validate_known_provider_values(
            &resource.id,
            schema,
            &crate::resource::attrs_to_hashmap(&resource.attributes),
            &resource.quoted_string_attrs,
            factories,
        )?;
    }
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    crate::value::canonicalize_data_sources_with_schemas(
        std::slice::from_mut(&mut resource),
        schemas,
    );
    for value in resource.attributes.values() {
        crate::resource::assert_value_fully_resolved(value)?;
    }
    Ok(ProviderReady(ResolvedDataSource::new(resource)))
}

/// Prepare a checked create request through the provider-boundary gate.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_create_request(
    resource: Resource,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<CreateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(
        resource,
        bindings,
        module_gate,
        provider_configs,
        normalizer,
        factories,
        schemas,
    )
    .await?;
    Ok(CreateRequest::checked(resource))
}

/// Prepare a checked update request through the provider-boundary gate.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_update_request(
    resource: Resource,
    from: crate::resource::State,
    changed_attributes: &[String],
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<UpdateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(
        resource,
        bindings,
        module_gate,
        provider_configs,
        normalizer,
        factories,
        schemas,
    )
    .await?;
    Ok(UpdateRequest::checked(from, changed_attributes, &resource))
}

/// Build the schema lookup used for provider-defined custom value types.
pub fn provider_custom_type_lookup(
    factories: &[Box<dyn ProviderFactory>],
) -> impl Fn(&TypeIdentity, &Value) -> Result<(), TypeError> + Send + Sync + '_ {
    move |identity, value| {
        let Some(text) = value
            .as_concrete()
            .and_then(|concrete| concrete.as_string_like())
        else {
            return Ok(());
        };
        for factory in factories {
            factory
                .validate_custom_type(identity, text)
                .map_err(|message| TypeError::ValidationFailed { message })?;
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
    let lookup = provider_custom_type_lookup(factories);
    let is_string_literal = |attribute: &str| quoted_string_attrs.contains(attribute);
    schema
        .validate_known_values_with_origins_and_lookup(attributes, &is_string_literal, &lookup)
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: id.clone(),
            message: errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; "),
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
    let lookup = provider_custom_type_lookup(factories);
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
            message: errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        })
}

fn unwrap_secret(value: Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => unwrap_secret(*inner),
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
