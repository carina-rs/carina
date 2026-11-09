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
//! let _: ProviderReadyResource = ProviderReady { value: resolved }; // private fields
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
use crate::provider::{
    HostNormalizer, HostResult, ProviderFailure, ProviderNormalizer, ProviderResult,
    merge_default_tags,
};
use crate::provider_text::{TextState, Unbound};
use crate::resource::{
    ConcreteValue, DeferredValue, ModuleSource, Resource, ResourceId, State,
    UnexpandedDependencyFields, Value, value_contains_unknown,
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
pub struct NormalizedResource<S = ModuleSource, D = UnexpandedDependencyFields>(
    Resource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>,
);

impl<S, D> NormalizedResource<S, D> {
    /// Borrow the normalized resource for read-only consumers.
    pub fn as_resource(
        &self,
    ) -> &Resource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>
    {
        &self.0
    }

    pub(super) fn into_resource(
        self,
    ) -> Resource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>
    {
        self.0
    }
}

/// Normalize one desired resource and return the typestate proof consumed by
/// executor patch builders.
async fn apply_desired_normalization_inner<S: Clone, D: Clone + Default, T: TextState>(
    resource: Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<T>,
    schemas: &SchemaRegistry,
) -> ProviderResult<NormalizedResource<S, D>, T> {
    let mut one = [resource];
    apply_desired_normalization_slice_generic(&mut one, provider_configs, normalizer, schemas)
        .await?;
    let [resource] = one;
    Ok(NormalizedResource(resource))
}

/// Provider-owned normalization through the shared desired pipeline.
pub async fn apply_desired_normalization<S: Clone, D: Clone + Default>(
    resource: Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<Unbound>,
    schemas: &SchemaRegistry,
) -> ProviderResult<NormalizedResource<S, D>> {
    apply_desired_normalization_inner(resource, provider_configs, normalizer, schemas).await
}

/// Host ingestion through the shared desired pipeline.
pub async fn apply_host_desired_normalization<S: Clone, D: Clone + Default>(
    resource: Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn HostNormalizer,
    schemas: &SchemaRegistry,
) -> HostResult<NormalizedResource<S, D>> {
    apply_desired_normalization_inner(resource, provider_configs, normalizer, schemas).await
}

/// Provider-owned normalization through the shared desired pipeline.
pub async fn apply_desired_normalization_slice<S: Clone, D: Clone + Default>(
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<Unbound>,
    schemas: &SchemaRegistry,
) -> ProviderResult<()> {
    apply_desired_normalization_slice_generic(resources, provider_configs, normalizer, schemas)
        .await
}

/// Host ingestion through the shared desired pipeline.
pub async fn apply_host_desired_normalization_slice<S: Clone, D: Clone + Default>(
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn HostNormalizer,
    schemas: &SchemaRegistry,
) -> HostResult<()> {
    apply_desired_normalization_slice_generic(resources, provider_configs, normalizer, schemas)
        .await
}

/// Dispatch gives normalizers main's plaintext input shape, then restores only
/// input annotations while host checks run. Newly introduced provider secrets
/// retain their original wire semantics.
pub(super) async fn apply_dispatch_desired_normalization<S: Clone, D: Clone + Default>(
    resource: Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn HostNormalizer,
    schemas: &SchemaRegistry,
) -> HostResult<(NormalizedResource<S, D>, RestoredSecrets)> {
    let mut resources = [resource];
    let mut restored = [RestoredSecrets::default()];
    apply_desired_normalization_slice_inner(
        &mut resources,
        provider_configs,
        normalizer,
        schemas,
        Some(&mut restored),
    )
    .await?;
    let [resource] = resources;
    let [restored] = restored;
    Ok((NormalizedResource(resource), restored))
}

#[derive(Clone)]
enum SecretSlot {
    Key(String),
    Index(usize),
}

#[derive(Default)]
pub(super) struct RestoredSecrets {
    paths: Vec<Vec<SecretSlot>>,
}

/// A correspondence is established once for the enclosing normalization
/// result. Descendants inherit the paired source/target cursor; there is no
/// independently supplied proof that could be reused for another subtree.
struct CorrespondingSecretValue<'a> {
    source: &'a Value,
    target: &'a mut Value,
}

impl CorrespondingSecretValue<'_> {
    fn restore(self, restored: &mut RestoredSecrets, path: &mut Vec<SecretSlot>) {
        match (self.source, self.target) {
            (Value::Deferred(DeferredValue::Secret(_)), target)
                if !matches!(target, Value::Deferred(DeferredValue::Secret(_))) =>
            {
                restored.protect(target, path);
            }
            (
                Value::Concrete(ConcreteValue::Map(source)),
                Value::Concrete(ConcreteValue::Map(target)),
            ) => {
                for (key, source) in source {
                    if let Some(target) = target.get_mut(key) {
                        path.push(SecretSlot::Key(key.clone()));
                        CorrespondingSecretValue { source, target }.restore(restored, path);
                        path.pop();
                    }
                }
            }
            (
                Value::Concrete(ConcreteValue::List(source)),
                Value::Concrete(ConcreteValue::List(target)),
            ) => {
                for (index, (source, target)) in source.iter().zip(target).enumerate() {
                    path.push(SecretSlot::Index(index));
                    CorrespondingSecretValue { source, target }.restore(restored, path);
                    path.pop();
                }
            }
            _ => {}
        }
    }
}

enum SecretRestoration<'a> {
    Corresponding {
        source: &'a indexmap::IndexMap<String, Value>,
        target: &'a mut indexmap::IndexMap<String, Value>,
    },
    Changed(&'a mut indexmap::IndexMap<String, Value>),
}

impl<'a> SecretRestoration<'a> {
    fn establish(
        source: &'a indexmap::IndexMap<String, Value>,
        target: &'a mut indexmap::IndexMap<String, Value>,
    ) -> Self {
        // A transformed/reordered result carries no correspondence. Compare
        // complete plaintext once, never again at each recursive child.
        let changed = source.values().any(crate::value::contains_secret)
            && plaintext_attributes(source) != plaintext_attributes(target);
        if changed {
            Self::Changed(target)
        } else {
            Self::Corresponding { source, target }
        }
    }

    fn restore(self, restored: &mut RestoredSecrets) {
        match self {
            Self::Changed(target) => {
                for (key, target) in target {
                    restored.protect(target, &[SecretSlot::Key(key.clone())]);
                }
            }
            Self::Corresponding { source, target } => {
                for (key, target) in target {
                    if let Some(source) = source.get(key) {
                        CorrespondingSecretValue { source, target }
                            .restore(restored, &mut vec![SecretSlot::Key(key.clone())]);
                    }
                }
            }
        }
    }
}

impl RestoredSecrets {
    fn protect(&mut self, target: &mut Value, path: &[SecretSlot]) {
        if !matches!(target, Value::Deferred(DeferredValue::Secret(_))) {
            *target = clone_plaintext_input(target).into_secret();
            self.paths.push(path.to_vec());
        }
    }

    fn restore_attributes(
        &mut self,
        source: &indexmap::IndexMap<String, Value>,
        target: &mut indexmap::IndexMap<String, Value>,
    ) {
        SecretRestoration::establish(source, target).restore(self);
    }

    pub(super) fn remove(&self, attributes: &mut indexmap::IndexMap<String, Value>) {
        self.apply(attributes, false);
    }

    fn rewrap(&self, attributes: &mut indexmap::IndexMap<String, Value>) {
        self.apply(attributes, true);
    }

    fn apply(&self, attributes: &mut indexmap::IndexMap<String, Value>, wrap: bool) {
        fn apply_at(value: &mut Value, path: &[SecretSlot], wrap: bool) {
            match path.split_first() {
                None => {
                    if wrap {
                        *value = value.clone().into_secret();
                    } else if let Value::Deferred(DeferredValue::Secret(payload)) = value {
                        *value = payload.expose_plaintext().clone();
                    }
                }
                Some((SecretSlot::Key(key), rest)) => {
                    if let Value::Concrete(ConcreteValue::Map(values)) = value
                        && let Some(value) = values.get_mut(key)
                    {
                        apply_at(value, rest, wrap);
                    }
                }
                Some((SecretSlot::Index(index), rest)) => {
                    if let Value::Concrete(ConcreteValue::List(values)) = value
                        && let Some(value) = values.get_mut(*index)
                    {
                        apply_at(value, rest, wrap);
                    }
                }
            }
        }
        for path in &self.paths {
            if let Some((SecretSlot::Key(key), rest)) = path.split_first()
                && let Some(value) = attributes.get_mut(key)
            {
                apply_at(value, rest, wrap);
            }
        }
    }
}

pub(crate) fn unwrap_dispatch_desired(resources: &mut [crate::resource::ProviderResource]) {
    for resource in resources {
        for value in resource.attributes.values_mut() {
            *value = unwrap_secret(value);
        }
    }
}

/// Main's dispatch projection, applied only after protected host checks.
pub(super) fn unwrap_secret(value: &Value) -> Value {
    #[cfg(test)]
    SECRET_RESTORATION_WORK.with(|work| {
        if let Some(count) = work.get() {
            work.set(Some(count + 1));
        }
    });
    match value {
        Value::Deferred(DeferredValue::Secret(payload)) => {
            unwrap_secret(payload.expose_plaintext())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(unwrap_secret(value)),
                default: Box::new(unwrap_secret(default)),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = unwrap_secret(expression.lhs());
            let rhs = unwrap_secret(expression.rhs());
            expression
                .evaluate_with_operands(lhs, rhs)
                .unwrap_or_else(|_| clone_plaintext_input(value))
        }
        Value::Concrete(ConcreteValue::List(values)) => Value::Concrete(ConcreteValue::List(
            values.iter().map(unwrap_secret).collect(),
        )),
        Value::Concrete(ConcreteValue::Map(values)) => {
            Value::Concrete(ConcreteValue::Map(plaintext_attributes(values)))
        }
        value => clone_plaintext_input(value),
    }
}

fn plaintext_attributes(
    values: &indexmap::IndexMap<String, Value>,
) -> indexmap::IndexMap<String, Value> {
    values
        .iter()
        .map(|(key, value)| (key.clone(), unwrap_secret(value)))
        .collect()
}

fn clone_plaintext_input(value: &Value) -> Value {
    #[cfg(test)]
    record_plaintext_copy(value);
    value.clone()
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
async fn apply_desired_normalization_slice_generic<S: Clone, D: Clone + Default, T: TextState>(
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<T>,
    schemas: &SchemaRegistry,
) -> ProviderResult<(), T> {
    apply_desired_normalization_slice_inner(resources, provider_configs, normalizer, schemas, None)
        .await
}

async fn apply_desired_normalization_slice_inner<S: Clone, D: Clone + Default, T: TextState>(
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<T>,
    schemas: &SchemaRegistry,
    restored: Option<&mut [RestoredSecrets]>,
) -> ProviderResult<(), T> {
    apply_desired_normalization_slice_unprojected(
        resources,
        provider_configs,
        normalizer,
        schemas,
        restored,
    )
    .await
}

async fn apply_desired_normalization_slice_unprojected<
    S: Clone,
    D: Clone + Default,
    T: TextState,
>(
    resources: &mut [Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    provider_configs: &[ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &dyn ProviderNormalizer<T>,
    schemas: &SchemaRegistry,
    mut restored: Option<&mut [RestoredSecrets]>,
) -> ProviderResult<(), T> {
    crate::value::canonicalize_resources_with_schemas(resources, schemas)?;
    let mut provider_resources = resources
        .iter()
        .cloned()
        .map(Resource::into_provider_resource)
        .collect::<Vec<_>>();
    let protected_inputs = restored.as_ref().map(|_| {
        provider_resources
            .iter()
            .map(|resource| resource.attributes.clone())
            .collect::<Vec<_>>()
    });
    if restored.is_some() {
        // The sealed host boundary must see annotations before the raw
        // normalizer receives dispatch's usual plaintext input shape.
        normalizer
            .normalize_dispatch_desired(&mut provider_resources)
            .await?;
    } else {
        normalizer
            .normalize_desired(&mut provider_resources)
            .await?;
    }
    if let (Some(restored), Some(inputs)) = (restored.as_deref_mut(), protected_inputs) {
        for ((resource, input), restored) in provider_resources.iter_mut().zip(inputs).zip(restored)
        {
            restored.restore_attributes(&input, &mut resource.attributes);
        }
    }
    for (resource, provider_resource) in resources.iter_mut().zip(provider_resources) {
        if resource.id != provider_resource.id {
            return Err(ProviderFailure::<T>::invalid_input(format!(
                "provider normalizer changed resource identity from `{}` to `{}`",
                resource.id, provider_resource.id
            )));
        }
        resource.apply_provider_resource(provider_resource);
    }
    reject_provider_boundary_block_aliases(resources, schemas)
        .map_err(crate::provider::HostError::host_for_state)?;
    crate::value::canonicalize_normalized_resources_with_schemas(resources, schemas)?;
    // Default-tag merging only inspects map shape and diagnoses config values.
    // Give it main's shape, then restore the diagnostic annotations for the
    // fully-resolved, ignore-path and wire gates that follow normalization.
    if let Some(restored) = restored.as_deref() {
        for (resource, restored) in resources.iter_mut().zip(restored) {
            restored.remove(&mut resource.attributes);
        }
    }
    for config in provider_configs {
        merge_default_tags(config, resources, schemas)
            .map_err(crate::provider::HostError::host_for_state)?;
    }
    if let Some(restored) = restored.as_deref() {
        for (resource, restored) in resources.iter_mut().zip(restored) {
            restored.rewrap(&mut resource.attributes);
        }
    }
    Ok(())
}

fn reject_provider_boundary_block_aliases<S, D>(
    resources: &[Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    schemas: &SchemaRegistry,
) -> HostResult<()> {
    for resource in resources {
        let Some(schema) = schemas.get_for(resource) else {
            continue;
        };
        let schema = schema.as_schema();
        if let Some(alias) = find_resource_block_name_alias(resource, schema) {
            return Err(crate::provider::HostError::invalid_input(format!(
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
pub fn states_contain_unknown<P: crate::resource::StateValueLeaf>(
    states: &HashMap<ResourceId, State<P>>,
) -> bool {
    states
        .values()
        .any(|state| state.attributes.values().any(value_contains_unknown))
}

#[cfg(test)]
thread_local! {
    static SECRET_RESTORATION_WORK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn record_plaintext_copy(value: &Value) {
    if SECRET_RESTORATION_WORK.get().is_none() {
        return;
    }
    SECRET_RESTORATION_WORK.set(Some(SECRET_RESTORATION_WORK.get().unwrap() + 1));
    match value {
        Value::Concrete(ConcreteValue::Map(values)) => {
            values.values().for_each(record_plaintext_copy)
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            values.iter().for_each(record_plaintext_copy)
        }
        Value::Deferred(DeferredValue::Secret(payload)) => {
            record_plaintext_copy(payload.expose_plaintext())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            record_plaintext_copy(value);
            record_plaintext_copy(default);
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            record_plaintext_copy(expression.lhs());
            record_plaintext_copy(expression.rhs());
        }
        _ => {}
    }
}

#[cfg(test)]
mod correspondence_tests {
    use super::*;

    #[test]
    fn unchanged_recursive_secret_restoration_has_linear_correspondence_work() {
        const DEPTH: usize = 96;
        for container in ["map", "list"] {
            let mut source =
                Value::Concrete(ConcreteValue::String("protected".into())).into_secret();
            for _ in 0..DEPTH {
                source = match container {
                    "map" => Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                        "child".into(),
                        source,
                    )]))),
                    "list" => Value::Concrete(ConcreteValue::List(vec![source])),
                    _ => unreachable!(),
                };
            }
            let plain = unwrap_secret(&source);
            let sources = indexmap::IndexMap::from([("objects".into(), source)]);
            let mut targets = indexmap::IndexMap::from([("objects".into(), plain)]);
            SECRET_RESTORATION_WORK.set(Some(0));
            let mut restored = RestoredSecrets::default();
            restored.restore_attributes(&sources, &mut targets);
            let work = SECRET_RESTORATION_WORK.replace(None).unwrap();
            assert_eq!(targets, sources, "annotations must be restored exactly");
            assert!(
                work <= (DEPTH + 3) * 4,
                "{container}: {work} plaintext node visits for depth {DEPTH}"
            );
        }
    }

    #[test]
    fn nested_secret_ancestors_have_a_linear_plaintext_copy_budget() {
        const DEPTH: usize = 96;
        let mut source = Value::Concrete(ConcreteValue::Bool(true)).into_secret();
        for _ in 0..DEPTH {
            source = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "child".into(),
                source,
            )])))
            .into_secret();
        }
        let plain = unwrap_secret(&source);
        let source = indexmap::IndexMap::from([("objects".into(), source)]);
        let mut target = indexmap::IndexMap::from([("objects".into(), plain)]);
        SECRET_RESTORATION_WORK.set(Some(0));
        RestoredSecrets::default().restore_attributes(&source, &mut target);
        let work = SECRET_RESTORATION_WORK.replace(None).unwrap();
        assert!(matches!(
            target["objects"],
            Value::Deferred(DeferredValue::Secret(_))
        ));
        assert_eq!(
            unwrap_secret(&source["objects"]),
            unwrap_secret(&target["objects"])
        );
        assert!(
            work <= (DEPTH + 3) * 4,
            "{work} node visits/copies for {DEPTH} protected ancestors"
        );
    }

    #[test]
    fn changed_collection_cannot_restore_secrets_by_position() {
        let secret = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "private-key".into(),
            Value::Concrete(ConcreteValue::Bool(true)),
        )])))
        .into_secret();
        let public = Value::Concrete(ConcreteValue::Bool(false));
        let source = Value::Concrete(ConcreteValue::List(vec![secret.clone(), public.clone()]));
        let target = Value::Concrete(ConcreteValue::List(vec![public, unwrap_secret(&secret)]));
        let mut restored = RestoredSecrets::default();
        let source = indexmap::IndexMap::from([("objects".into(), source)]);
        let mut target_attributes = indexmap::IndexMap::from([("objects".into(), target)]);
        restored.restore_attributes(&source, &mut target_attributes);
        assert!(
            matches!(
                target_attributes["objects"],
                Value::Deferred(DeferredValue::Secret(_))
            ),
            "without carried correspondence the complete changed collection must remain protected"
        );
    }
}
