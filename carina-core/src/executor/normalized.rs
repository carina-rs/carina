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
//! use carina_core::executor::compute_full_diff_patch;
//! use carina_core::resource::{Resource, State};
//! let from: State = unimplemented!();
//! let to: Resource = unimplemented!();
//! compute_full_diff_patch(&from, &to);   // must not compile
//! ```

use std::collections::HashMap;

use crate::parser::ProviderConfig;
use crate::provider::{ProviderFactory, ProviderNormalizer};
use crate::resource::{
    ConcreteValue, DeferredValue, InterpolationPart, Resource, ResourceId, State, Value,
    contains_resource_ref,
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
}

/// Normalize one desired resource and return the typestate proof consumed by
/// executor patch builders.
pub async fn apply_desired_normalization(
    resource: Resource,
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) -> NormalizedResource {
    let mut one = [resource];
    apply_desired_normalization_slice(&mut one, provider_configs, normalizer, factories, schemas)
        .await;
    let [resource] = one;
    NormalizedResource(resource)
}

/// Apply the full desired-side pipeline to a resource slice in place:
/// canonicalize, strip provider-boundary-deferred attributes, run desired
/// normalization stages, then restore stripped attributes.
///
/// Use this from callers that own a slice and do not need to interleave
/// plan-only state-side passes. `PlanPreprocessor::prepare` interleaves those
/// passes, so it orchestrates these same steps itself.
pub async fn apply_desired_normalization_slice(
    resources: &mut [Resource],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) {
    crate::value::canonicalize_resources_with_schemas(resources, schemas);
    let stripped = strip_provider_boundary_attributes(resources);
    run_desired_normalization_stages(resources, provider_configs, normalizer, factories, schemas)
        .await;
    restore_stripped_attributes(resources, stripped);
}

/// Run the desired-side normalization stages on a slice that has already been
/// canonicalized and whose unserializable attributes have been stripped.
///
/// **Most callers want [`apply_desired_normalization_slice`] instead**, which
/// wraps canonicalize, strip, these stages, and restore in one call.
///
/// `run_desired_normalization_stages` exists only for callers that need to
/// interleave plan-only state-side passes (for example,
/// `PlanPreprocessor::prepare` in `carina-cli`) between strip and restore. If
/// that does not describe your call site, use
/// [`apply_desired_normalization_slice`].
///
/// Stages, in order:
///
/// 1. [`ProviderNormalizer::normalize_desired`]
/// 2. [`ProviderNormalizer::merge_default_tags`] for each [`ProviderConfig`]
///    with non-empty `default_tags`
/// 3. `resolve_enum_aliases_for_resources`
///
/// Caller responsibilities:
///
/// - Run `canonicalize_resources_with_schemas` on the slice before calling
///   this function. The single-resource wrapper
///   [`apply_desired_normalization`] does this for you; plan-time
///   `PlanPreprocessor::prepare` does this itself before stripping deferred
///   values.
/// - Strip attributes that the WASM provider boundary refuses to serialize
///   (`Value::Deferred(DeferredValue::Unknown)` and ref-bearing attributes)
///   before calling, then restore them after. Use
///   [`strip_provider_boundary_attributes`] and
///   [`restore_stripped_attributes`] for that wrapper.
///
/// The single-resource [`apply_desired_normalization`] wraps this function
/// with canonicalize plus strip/restore for callers that own one resource by
/// value, including the apply executor's per-effect resolve path and the
/// state-backend bootstrap path.
pub async fn run_desired_normalization_stages(
    resources: &mut [Resource],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) {
    normalizer.normalize_desired(resources).await;
    for config in provider_configs {
        if !config.default_tags.is_empty() {
            normalizer
                .merge_default_tags(resources, &config.default_tags, schemas)
                .await;
        }
    }
    crate::value::resolve_enum_aliases_for_resources(resources, factories);
}

/// One stripped attribute retained so it can be reinserted at its original
/// position after provider-facing desired normalization returns.
pub struct StrippedAttribute {
    pub(crate) insert_index: usize,
    pub(crate) key: String,
    pub(crate) value: Value,
}

/// Attributes removed from a resource slice by [`strip_attributes_matching`].
pub struct StrippedAttributes(HashMap<ResourceId, Vec<StrippedAttribute>>);

impl StrippedAttributes {
    /// Number of resources that had at least one attribute stripped.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no attributes were stripped from any resource.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Read-only stripped entry groups, one group per resource.
    pub fn values(&self) -> impl Iterator<Item = &[StrippedAttribute]> {
        self.0.values().map(Vec::as_slice)
    }
}

/// Remove every attribute whose value matches `predicate` from each resource's
/// attribute map, retaining the original `IndexMap` positions for restoration.
pub(crate) fn strip_attributes_matching(
    resources: &mut [Resource],
    predicate: &dyn Fn(&Value) -> bool,
) -> StrippedAttributes {
    let mut out: HashMap<ResourceId, Vec<StrippedAttribute>> = HashMap::new();
    for resource in resources.iter_mut() {
        let mut stripped = Vec::new();
        let to_remove: Vec<(usize, String)> = resource
            .attributes
            .iter()
            .enumerate()
            .filter(|(_, (_, value))| predicate(value))
            .map(|(i, (key, _))| (i, key.clone()))
            .collect();

        for (insert_index, key) in to_remove.into_iter().rev() {
            if let Some(value) = resource.attributes.shift_remove(&key) {
                stripped.push(StrippedAttribute {
                    insert_index,
                    key,
                    value,
                });
            }
        }

        if !stripped.is_empty() {
            stripped.sort_by_key(|entry| entry.insert_index);
            out.insert(resource.id.clone(), stripped);
        }
    }
    StrippedAttributes(out)
}

/// Strip every resource attribute that the provider boundary refuses to
/// serialize: deferred unknowns and values that recursively contain resource
/// references.
pub fn strip_provider_boundary_attributes(resources: &mut [Resource]) -> StrippedAttributes {
    strip_attributes_matching(resources, &|value| {
        value_contains_unknown(value) || contains_resource_ref(value)
    })
}

/// Reinsert attributes removed by [`strip_attributes_matching`] at their
/// original positions.
pub fn restore_stripped_attributes(resources: &mut [Resource], mut stripped: StrippedAttributes) {
    for resource in resources.iter_mut() {
        if let Some(entries) = stripped.0.remove(&resource.id) {
            for entry in entries {
                let target = entry.insert_index.min(resource.attributes.len());
                resource
                    .attributes
                    .shift_insert(target, entry.key, entry.value);
            }
        }
    }
}

/// Return whether any state attribute recursively contains
/// `Value::Deferred(DeferredValue::Unknown)`.
pub fn states_contain_unknown(states: &HashMap<ResourceId, State>) -> bool {
    states
        .values()
        .any(|state| state.attributes.values().any(value_contains_unknown))
}

/// Return whether `value` recursively contains
/// `Value::Deferred(DeferredValue::Unknown)`.
pub(crate) fn value_contains_unknown(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(_)) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(value_contains_unknown),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(value_contains_unknown),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                InterpolationPart::Expr(value) if value_contains_unknown(value)
            )
        }),
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            args.iter().any(value_contains_unknown)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_unknown(inner),
        _ => false,
    }
}
