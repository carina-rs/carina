//! Checked provider-boundary preparation.
//!
//! [`ProviderReady`] is the capability required by provider requests. Its
//! payload is private and this module is the only place that constructs it,
//! so normalization or identity resolution alone cannot cross the provider
//! boundary without value-constraint validation.

use std::ops::Deref;

use thiserror::Error;

use crate::executor::normalized::apply_desired_normalization;
use crate::parser::ProviderConfig;
use crate::provider::{
    CreateRequest, ProviderFactory, ProviderNormalizer, UpdateRequest, build_update_patch,
};
use crate::resource::{
    ConcreteValue, DeferredValue, ResolvedResource, Resource, ResourceId, Value,
};
use crate::schema::{SchemaRegistry, TypeError, TypeIdentity};
use crate::value::SerializationError;

/// An immutable value that passed the provider-boundary constraint gate.
///
/// The inner field is deliberately private. There is no `new`, `From`,
/// `Default`, or deserialization path; callers obtain this capability only
/// from [`prepare_provider_ready_resource`].
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
}

/// Validate and prepare a resource for provider dispatch.
///
/// This is the sole constructor for [`ProviderReadyResource`]. It unwraps
/// secrets for the provider, re-runs the complete desired-side normalization
/// pipeline, validates every provider-facing schema value (including provider
/// custom types), and finally proves no deferred placeholder remains.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_provider_ready_resource(
    mut resource: Resource,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    for value in resource.attributes.values_mut() {
        *value = unwrap_secret(value.clone());
    }
    let normalized =
        apply_desired_normalization(resource, provider_configs, normalizer, factories, schemas)
            .await;
    let resource = normalized.into_resource();
    validate_known_resource_values(&resource, factories, schemas)?;
    crate::resource::assert_resource_fully_resolved(&resource)?;
    Ok(ProviderReady(ResolvedResource::new(resource)))
}

/// Prepare a checked create request through the provider-boundary gate.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_create_request(
    resource: Resource,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<CreateRequest, ProviderPreparationError> {
    let resource =
        prepare_provider_ready_resource(resource, provider_configs, normalizer, factories, schemas)
            .await?;
    Ok(CreateRequest { resource })
}

/// Prepare a checked update request through the provider-boundary gate.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_update_request(
    resource: Resource,
    from: crate::resource::State,
    changed_attributes: &[String],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> Result<UpdateRequest, ProviderPreparationError> {
    let resource =
        prepare_provider_ready_resource(resource, provider_configs, normalizer, factories, schemas)
            .await?;
    let patch = build_update_patch(changed_attributes, &resource, &from);
    Ok(UpdateRequest { from, patch })
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
