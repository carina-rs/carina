//! Desired-side resource normalization typestate.
//!
//! ```compile_fail
//! use carina_core::provider::build_update_patch;
//! use carina_core::resource::{Resource, State};
//! use carina_core::schema::ResourceSchema;
//! let r: Resource = unimplemented!();
//! let s: State = unimplemented!();
//! let schema = ResourceSchema::new("test");
//! build_update_patch(&[], &r, &s, &schema);   // must not compile
//! ```
//!
//! ```compile_fail
//! use carina_core::provider::CreateRequest;
//! use carina_core::resource::Resource;
//! let r: Resource = unimplemented!();
//! let _ = CreateRequest { resource: r };   // must not compile
//! ```
//!
//! ```compile_fail
//! use carina_core::provider::CreateRequest;
//! use carina_core::resource::{ResolvedResource, Resource};
//! let resource = Resource::new("test", "example");
//! let resolved = ResolvedResource::new(resource);
//! let _ = CreateRequest { resource: resolved }; // checked witness required
//! ```
//!
//! ```compile_fail
//! use carina_core::provider::build_update_patch;
//! use carina_core::resource::{ResolvedResource, Resource, State};
//! use carina_core::schema::ResourceSchema;
//! let resource = Resource::new("test", "example");
//! let resolved = ResolvedResource::new(resource);
//! let state: State = unimplemented!();
//! let schema = ResourceSchema::new("test");
//! let _ = build_update_patch(&[], &resolved, &state, &schema); // checked witness required
//! ```
//!
//! ```compile_fail
//! use carina_core::executor::normalized::NormalizedResource;
//! use carina_core::provider::ProviderReadyResource;
//!
//! fn bypass_value_checks(normalized: NormalizedResource) -> ProviderReadyResource {
//!     normalized.into() // no unchecked normalization-to-provider conversion
//! }
//! ```
//!
//! ```compile_fail
//! use carina_core::provider::{ProviderReady, ProviderReadyResource};
//! use carina_core::resource::{ResolvedResource, Resource};
//! let resolved = ResolvedResource::new(Resource::new("test", "example"));
//! let _: ProviderReadyResource = ProviderReady(resolved); // private field
//! ```
//!
//! ```compile_fail
//! use carina_core::provider::ProviderReadyResource;
//! use carina_core::resource::{ResolvedResource, Resource};
//! let resolved = ResolvedResource::new(Resource::new("test", "example"));
//! let _: ProviderReadyResource = resolved.into(); // no unchecked conversion
//! ```

use std::collections::HashMap;

use crate::parser::ProviderConfig;
use crate::provider::{ProviderNormalizer, ProviderResult, merge_default_tags};
use crate::resource::{
    ConcreteValue, DeferredValue, ModuleSource, Resource, ResourceId, State, Value,
    value_contains_unknown,
};
use crate::schema::{SchemaRegistry, find_resource_block_name_alias};

/// A `Resource` that has been through the full plan-time desired-side
/// normalization pipeline.
///
/// Parsed block aliases are canonicalized for both the evaluated and authored
/// snapshots at the wiring boundary, and provider normalizers are rejected if
/// they introduce one again. Consequently every schema-known map behind this
/// wrapper uses canonical attribute names.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedResource<S = ModuleSource>(Resource<S>);

impl<S> NormalizedResource<S> {
    /// Borrow the normalized resource for read-only consumers.
    pub fn as_resource(&self) -> &Resource<S> {
        &self.0
    }

    pub(super) fn into_resource(self) -> Resource<S> {
        self.0
    }
}

/// Normalize one desired resource and return the typestate proof consumed by
/// executor patch builders.
pub async fn apply_desired_normalization<S: Clone>(
    resource: Resource<S>,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    schemas: &SchemaRegistry,
) -> ProviderResult<NormalizedResource<S>> {
    let mut one = [resource];
    apply_desired_normalization_slice(&mut one, provider_configs, normalizer, schemas).await?;
    let [resource] = one;
    Ok(NormalizedResource(resource))
}

/// Apply the full desired-side pipeline to a resource slice in place.
///
/// Stages run in order:
///
/// 1. Canonicalize resource values with their schemas.
/// 2. Run [`ProviderNormalizer::normalize_desired`].
/// 3. Reject block-name aliases introduced by the provider normalizer.
/// 4. Re-canonicalize guest-returned enum strings using state-text semantics.
/// 5. Run [`merge_default_tags`] for each provider instance.
///
/// An error stops the pipeline before later stages run.
pub async fn apply_desired_normalization_slice<S: Clone>(
    resources: &mut [Resource<S>],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    schemas: &SchemaRegistry,
) -> ProviderResult<()> {
    crate::value::canonicalize_resources_with_schemas(resources, schemas)?;
    let mut provider_resources = resources
        .iter()
        .cloned()
        .map(Resource::into_provider_resource)
        .collect::<Vec<_>>();
    normalizer
        .normalize_desired(&mut provider_resources)
        .await?;
    for (resource, provider_resource) in resources.iter_mut().zip(provider_resources) {
        if resource.id != provider_resource.id {
            return Err(crate::provider::ProviderError::invalid_input(format!(
                "provider normalizer changed resource identity from `{}` to `{}`",
                resource.id, provider_resource.id
            )));
        }
        resource.apply_provider_resource(provider_resource);
    }
    reject_provider_boundary_block_aliases(resources, schemas)?;
    crate::value::canonicalize_normalized_resources_with_schemas(resources, schemas)?;
    for config in provider_configs {
        merge_default_tags(config, resources, schemas)?;
    }
    Ok(())
}

fn reject_provider_boundary_block_aliases<S>(
    resources: &[Resource<S>],
    schemas: &SchemaRegistry,
) -> ProviderResult<()> {
    for resource in resources {
        let Some(schema) = schemas.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();
        if let Some(alias) = find_resource_block_name_alias(resource, schema) {
            return Err(crate::provider::ProviderError::invalid_input(format!(
                "{}: block-name alias `{}` reached the provider boundary at `{}`; expected canonical attribute `{}`",
                alias.resource_id(),
                alias.alias(),
                alias.path(),
                alias.canonical(),
            )));
        }
    }
    Ok(())
}

/// Return whether any state attribute recursively contains
/// `Value::Deferred(DeferredValue::Unknown)`.
pub fn states_contain_unknown(states: &HashMap<ResourceId, State>) -> bool {
    states
        .values()
        .any(|state| state.attributes.values().any(value_contains_unknown))
}

/// Return whether `value` can drive plan-time deferred-for expansion.
///
/// Expansion needs a fully known iterable. Any reference-bearing or
/// unevaluated deferred shape means substitution must wait until apply
/// time, while concrete containers are walked recursively so deferred
/// leaves nested inside lists/maps are rejected by the same predicate.
pub fn is_value_fully_concrete_for_expansion(value: &Value) -> bool {
    match value {
        Value::Concrete(concrete) => match concrete {
            ConcreteValue::String(_) => true,
            ConcreteValue::EnumIdentifier(_) => true,
            ConcreteValue::CanonicalEnum(_) => true,
            ConcreteValue::Int(_) => true,
            ConcreteValue::Float(_) => true,
            ConcreteValue::Bool(_) => true,
            ConcreteValue::Duration(_) => true,
            ConcreteValue::List(items) => items.iter().all(is_value_fully_concrete_for_expansion),
            ConcreteValue::StringList(_) => true,
            ConcreteValue::Map(map) => map.values().all(is_value_fully_concrete_for_expansion),
        },
        Value::Deferred(deferred) => match deferred {
            DeferredValue::ResourceRef { .. } => false,
            DeferredValue::BindingRef { .. } => false,
            DeferredValue::Coalesce { .. } => false,
            DeferredValue::Interpolation(_) => false,
            DeferredValue::FunctionCall { .. } => false,
            DeferredValue::Unavailable { .. } => false,
            DeferredValue::ResolvedFunctionCall { .. } => false,
            DeferredValue::Access(_) => false,
            DeferredValue::Secret(inner) => is_value_fully_concrete_for_expansion(inner),
            DeferredValue::RedactedSecret { .. } => false,
            DeferredValue::Unknown(_) => false,
        },
    }
}
