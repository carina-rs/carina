//! Desired-side resource normalization typestate.
//!
//! ```compile_fail
//! use carina_core::provider::build_update_patch;
//! use carina_core::resource::{Resource, State};
//! let r: Resource = unimplemented!();
//! let s: State = unimplemented!();
//! build_update_patch(&[], &r, &s);   // must not compile
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
//! let resource = Resource::new("test", "example");
//! let resolved = ResolvedResource::new(resource);
//! let state: State = unimplemented!();
//! let _ = build_update_patch(&[], &resolved, &state); // checked witness required
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
//!
//! ```compile_fail
//! use carina_core::executor::compute_full_diff_patch;
//! use carina_core::resource::{Resource, State};
//! let from: State = unimplemented!();
//! let to: Resource = unimplemented!();
//! compute_full_diff_patch(&from, &to);   // must not compile: raw Resource has no schema-aware diff
//! ```

use std::collections::HashMap;

use crate::parser::ProviderConfig;
use crate::provider::{ProviderNormalizer, ProviderResult, merge_default_tags};
use crate::resource::{
    ConcreteValue, DeferredValue, Resource, ResourceId, State, Value, value_contains_unknown,
};
use crate::schema::SchemaRegistry;

/// A `Resource` that has been through the full plan-time desired-side
/// normalization pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedResource(Resource);

impl NormalizedResource {
    /// Borrow the normalized resource for read-only consumers.
    pub fn as_resource(&self) -> &Resource {
        &self.0
    }

    pub(super) fn into_resource(self) -> Resource {
        self.0
    }
}

/// Normalize one desired resource and return the typestate proof consumed by
/// executor patch builders.
pub async fn apply_desired_normalization(
    resource: Resource,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    schemas: &SchemaRegistry,
) -> ProviderResult<NormalizedResource> {
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
/// 3. Re-canonicalize guest-returned enum strings using state-text semantics.
/// 4. Run [`merge_default_tags`] for each provider instance.
///
/// An error stops the pipeline before later stages run.
pub async fn apply_desired_normalization_slice(
    resources: &mut [Resource],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    schemas: &SchemaRegistry,
) -> ProviderResult<()> {
    crate::value::canonicalize_resources_with_schemas(resources, schemas)?;
    normalizer.normalize_desired(resources).await?;
    crate::value::canonicalize_normalized_resources_with_schemas(resources, schemas)?;
    for config in provider_configs {
        merge_default_tags(config, resources, schemas)?;
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
            DeferredValue::Interpolation(_) => false,
            DeferredValue::FunctionCall { .. } => false,
            DeferredValue::Access(_) => false,
            DeferredValue::Secret(inner) => is_value_fully_concrete_for_expansion(inner),
            DeferredValue::RedactedSecret { .. } => false,
            DeferredValue::Unknown(_) => false,
        },
    }
}
