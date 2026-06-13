//! Desired-side resource normalization typestate.
//!
//! ```compile_fail
//! use carina_core::provider::build_update_patch;
//! use carina_core::resource::{Resource, State};
//! let r: Resource = unimplemented!();
//! let s: State = unimplemented!();
//! build_update_patch(&[], &r, &s);   // must not compile
//! ```

use crate::parser::ProviderConfig;
use crate::provider::{ProviderFactory, ProviderNormalizer};
use crate::resource::Resource;
use crate::schema::SchemaRegistry;

/// A `Resource` that has been through the full plan-time desired-side
/// normalization pipeline.
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
    apply_desired_normalization_in_place(
        &mut one,
        provider_configs,
        normalizer,
        factories,
        schemas,
    )
    .await;
    let [resource] = one;
    NormalizedResource(resource)
}

/// Run the shared desired-side normalization stages in place for plan-time
/// callers that already own a resource slice.
pub async fn apply_desired_normalization_in_place(
    resources: &mut [Resource],
    provider_configs: &[ProviderConfig],
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn ProviderFactory>],
    schemas: &SchemaRegistry,
) {
    crate::value::canonicalize_resources_with_schemas(resources, schemas);
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
