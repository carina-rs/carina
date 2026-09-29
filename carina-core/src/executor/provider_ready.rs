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
use crate::module_resolver::{ConstraintEvaluation, evaluate_constraint};
use crate::parser::ProviderConfig;
use crate::provider::{
    CreateRequest, ProviderFactory, ProviderNormalizer, UpdateRequest, build_update_patch,
};
use crate::resource::{
    Composition, ConcreteValue, DataSource, DeferredValue, ModuleSource, ResolvedDataSource,
    ResolvedResource, Resource, ResourceId, Value,
};
use crate::schema::{SchemaRegistry, TypeError, TypeIdentity};
use crate::value::SerializationError;

/// An immutable value that passed the provider-boundary constraint gate.
///
/// The inner field is deliberately private. There is no `new`, `From`,
/// `Default`, or deserialization path; callers obtain this capability only
/// from [`prepare_provider_ready_resource`].
///
/// ```compile_fail
/// use carina_core::provider::ProviderReadyResource;
/// use carina_core::resource::{ResolvedResource, Resource};
///
/// let resolved = ResolvedResource::new(Resource::new("test", "example"));
/// let _ready = ProviderReadyResource(resolved);
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

/// One module constraint that failed at an apply-time value boundary.
///
/// Actual values are rendered eagerly with the secret-aware value formatter;
/// this type never retains a [`Value`] and is therefore safe to display or
/// debug-log.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleConstraintFailure {
    composition_id: String,
    constraint_id: String,
    pub module: String,
    pub instance: String,
    pub arguments: Vec<String>,
    pub message: String,
    pub actuals: Vec<(String, String)>,
    pub detail: Option<String>,
}

impl fmt::Display for ModuleConstraintFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let arguments = self
            .arguments
            .iter()
            .map(|argument| format!("`{argument}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let actuals = self
            .actuals
            .iter()
            .map(|(argument, value)| format!("{argument} = {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "module `{}` instance `{}` constraint failed for argument(s) {}: {}; values: {}",
            self.module, self.instance, arguments, self.message, actuals
        )?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
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
/// Every call resolves and evaluates the instance's pending constraints from
/// the bindings supplied to that call. There is deliberately no successful
/// result cache or value fingerprint: a later binding change must always be
/// observed. The small reporting set only prevents a terminal sweep from
/// emitting the exact same already-reported violation a second time; it is
/// never consulted to skip evaluation.
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

    /// Re-evaluate constraints for the module instance that owns a managed
    /// resource immediately before provider preparation.
    pub fn check_resource(
        &self,
        resource: &Resource,
        bindings: &ResolvedBindings,
    ) -> Result<(), ModuleConstraintGateError> {
        self.check_source(resource.module_source.as_ref(), bindings)
    }

    /// Re-evaluate constraints for the module instance that owns a data source
    /// immediately before provider preparation.
    pub fn check_data_source(
        &self,
        resource: &DataSource,
        bindings: &ResolvedBindings,
    ) -> Result<(), ModuleConstraintGateError> {
        self.check_source(resource.module_source.as_ref(), bindings)
    }

    /// Evaluate every remaining constraint at the end of apply.
    ///
    /// A constraint whose arguments are still unresolved is an explicit
    /// failure here. Failures already emitted by a consuming provider effect
    /// are re-evaluated but omitted from this returned diagnostic only when
    /// the complete rendered failure is unchanged.
    pub fn finish(&self, bindings: &ResolvedBindings) -> Result<(), ModuleConstraintGateError> {
        let failures = self
            .compositions
            .iter()
            .flat_map(|composition| evaluate_composition(composition, bindings, true))
            .collect::<Vec<_>>();
        self.report_new(failures)
    }

    fn check_source(
        &self,
        source: Option<&ModuleSource>,
        bindings: &ResolvedBindings,
    ) -> Result<(), ModuleConstraintGateError> {
        let Some(ModuleSource::Module { name, instance }) = source else {
            return Ok(());
        };
        let failures = self
            .compositions
            .iter()
            .filter(|composition| {
                composition.instance == *instance && composition.module_name == *name
            })
            .flat_map(|composition| evaluate_composition(composition, bindings, false))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            return Ok(());
        }
        self.reported_failures
            .lock()
            .expect("module constraint reporting lock poisoned")
            .extend(failures.iter().cloned());
        Err(ModuleConstraintGateError { failures })
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

fn evaluate_composition(
    composition: &Composition,
    bindings: &ResolvedBindings,
    terminal: bool,
) -> Vec<ModuleConstraintFailure> {
    let mut resolved_arguments = HashMap::new();
    let mut resolution_errors = HashMap::new();
    for (name, argument) in &composition.signature.arguments {
        match crate::resolver::resolve_ref_value(argument.value(), bindings) {
            Ok(value) => {
                resolved_arguments.insert(name.clone(), value);
            }
            Err(error) => {
                resolution_errors.insert(name.clone(), error);
                resolved_arguments.insert(name.clone(), argument.value().clone());
            }
        }
    }

    composition
        .signature
        .pending_constraints
        .iter()
        .filter_map(|constraint| {
            let mut arguments = constraint.referenced_arguments().to_vec();
            arguments.sort();
            arguments.dedup();
            let failure =
                |message: String, actuals: Vec<(String, String)>, detail: Option<String>| {
                    ModuleConstraintFailure {
                        composition_id: composition.id.to_string(),
                        constraint_id: constraint.id().as_str().to_string(),
                        module: composition.module_name.clone(),
                        instance: composition.instance.clone(),
                        arguments: arguments.clone(),
                        message,
                        actuals,
                        detail,
                    }
                };
            let actuals = || constraint_actuals(&arguments, &resolved_arguments);

            if let Some((name, error)) = arguments
                .iter()
                .find_map(|name| resolution_errors.get(name).map(|error| (name, error)))
            {
                return Some(failure(
                    constraint.message().to_string(),
                    actuals(),
                    Some(format!("could not resolve argument `{name}`: {error}")),
                ));
            }

            match evaluate_constraint(
                constraint.expression(),
                &resolved_arguments,
                constraint.message(),
            ) {
                Ok(ConstraintEvaluation::Satisfied) => None,
                Ok(ConstraintEvaluation::Pending) if !terminal => None,
                Ok(ConstraintEvaluation::Pending) => Some(failure(
                    constraint.message().to_string(),
                    actuals(),
                    Some("constraint inputs are still unresolved at end of apply".to_string()),
                )),
                Ok(ConstraintEvaluation::Violated(violation)) => {
                    Some(failure(violation.message, violation.actuals, None))
                }
                Err(error) => Some(failure(
                    constraint.message().to_string(),
                    actuals(),
                    Some(error),
                )),
            }
        })
        .collect()
}

fn constraint_actuals(
    arguments: &[String],
    values: &HashMap<String, Value>,
) -> Vec<(String, String)> {
    arguments
        .iter()
        .map(|name| {
            let rendered = values
                .get(name)
                .map(crate::value::format_value)
                .unwrap_or_else(|| "<missing>".to_string());
            (name.clone(), rendered)
        })
        .collect()
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
    mut resource: Resource,
    bindings: &ResolvedBindings,
    module_gate: &ModuleConstraintGate,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    module_gate.check_resource(&resource, bindings)?;
    validate_known_resource_values(&resource, factories, schemas)?;
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
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    module_gate.check_data_source(&resource, bindings)?;
    validate_known_data_source_values(&resource, factories, schemas)?;
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
    Ok(CreateRequest { resource })
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
    let patch = build_update_patch(changed_attributes, &resource, &from);
    Ok(UpdateRequest::checked(from, patch, &resource))
}

fn validate_known_resource_values(
    resource: &Resource,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<(), ProviderPreparationError> {
    let Some(schema) = schemas.get_for(resource) else {
        return Ok(());
    };
    let mut attributes = resource.resolved_attributes();
    for value in attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    let lookup = |identity: &TypeIdentity, value: &Value| {
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
    };
    let is_string_literal = |attribute: &str| resource.quoted_string_attrs.contains(attribute);
    schema
        .validate_known_values_with_origins_and_lookup(&attributes, &is_string_literal, &lookup)
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: resource.id.clone(),
            message: errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        })
}

fn validate_known_data_source_values(
    resource: &DataSource,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<(), ProviderPreparationError> {
    let Some(schema) = schemas.get_for_data_source(resource) else {
        return Ok(());
    };
    let attributes = crate::resource::attrs_to_hashmap(&resource.attributes);
    let lookup = |identity: &TypeIdentity, value: &Value| {
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
    };
    let is_string_literal = |attribute: &str| resource.quoted_string_attrs.contains(attribute);
    schema
        .validate_known_values_with_origins_and_lookup(&attributes, &is_string_literal, &lookup)
        .map_err(|errors| ProviderPreparationError::ValueConstraint {
            resource: resource.id.clone(),
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
