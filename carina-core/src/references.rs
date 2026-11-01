//! Comparison-only, guest-resolved reference identities. Raw values never change.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::provider::{LiftedSavedAttrs, ProviderError, ProviderInstanceKey, ProviderRouter};
use crate::resource::{
    ConcreteValue, Current, PlanInputCurrent, ResolvedResource, ResourceId, Value,
};
use crate::schema::{SchemaRegistry, Shape, TypeIdentity};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReferenceRequest {
    pub ty: TypeIdentity,
    pub value: String,
}

/// A canonical comparison key, constructible only by decoding a guest's
/// `Resolved` outcome. It has no value/state/payload conversion.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalReference(String);

#[derive(Debug)]
pub enum ReferenceOutcome {
    Resolved(CanonicalReference),
    NotFound(ProviderError),
}

#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("reference access denied")] // error-chain: caller-renders-sources
    AccessDenied(#[source] ProviderError),
    #[error("reference resolution temporarily failed")] // error-chain: caller-renders-sources
    Transient(#[source] ProviderError),
    #[error("invalid reference")] // error-chain: caller-renders-sources
    InvalidReference(#[source] ProviderError),
}

impl ReferenceOutcome {
    pub fn from_guest(
        outcome: carina_provider_protocol::types::ReferenceOutcome,
    ) -> Result<Self, ReferenceError> {
        use carina_provider_protocol::types::ReferenceOutcome as Guest;
        match outcome {
            Guest::Resolved(value) if value.is_empty() => Err(ReferenceError::InvalidReference(
                ProviderError::invalid_input("guest returned an empty canonical reference"),
            )),
            // The guest owns canonical format validation (for example, ARN
            // shape). Core/host validate only non-emptiness and batch length;
            // interpreting an AWS ARN here would make this seam provider-specific.
            Guest::Resolved(value) => Ok(Self::Resolved(CanonicalReference(value))),
            Guest::NotFound(error) => Ok(Self::NotFound(ProviderError::from_wire(*error))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredSide {
    Canonical(CanonicalReference),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentSide {
    Canonical(CanonicalReference),
    Dangling,
}

#[derive(Debug, thiserror::Error)]
pub enum ReferenceFailure {
    #[error("desired reference does not resolve")] // error-chain: caller-renders-sources
    DesiredNotFound(#[source] Arc<ProviderError>),
    #[error(transparent)]
    Resolution(#[from] ReferenceError),
    #[error("provider reference resolution call failed")] // error-chain: caller-renders-sources
    Call(#[source] ProviderError),
    #[error("reference comparison was not prepared")]
    MissingWitness,
}

#[derive(Debug, thiserror::Error)]
#[error("provider instance {instance:?}, resource {resource}, attribute {path}, reference {raw:?}")] // error-chain: caller-renders-sources
pub struct ReferencePlanError {
    pub instance: ProviderInstanceKey,
    pub resource: ResourceId,
    pub path: String,
    pub raw: String,
    #[source]
    pub source: ReferenceFailure,
}

impl From<Box<ReferencePlanError>> for ProviderError {
    fn from(error: Box<ReferencePlanError>) -> Self {
        ProviderError::invalid_input_caused_by("Failed to prepare reference comparison", error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    instance: ProviderInstanceKey,
    ty: TypeIdentity,
    raw: String,
}

#[derive(Debug, Clone)]
struct Occurrence {
    resource: ResourceId,
    path: String,
    request: ReferenceRequest,
    desired: bool,
}

impl Occurrence {
    fn key(&self) -> CacheKey {
        CacheKey {
            instance: ProviderInstanceKey::from(&self.resource),
            ty: self.request.ty.clone(),
            raw: self.request.value.clone(),
        }
    }

    fn error(&self, source: ReferenceFailure) -> Box<ReferencePlanError> {
        Box::new(ReferencePlanError {
            instance: ProviderInstanceKey::from(&self.resource),
            resource: self.resource.clone(),
            path: self.path.clone(),
            raw: self.request.value.clone(),
            source,
        })
    }
}

/// Evidence that the reference pairs used by one planning run were resolved.
/// Only preparation constructs this witness. Errors cannot enter its side maps.
#[derive(Debug, Clone)]
pub struct ResolvedReferences {
    desired: HashMap<CacheKey, DesiredSide>,
    current: HashMap<CacheKey, CurrentSide>,
    dangling: HashMap<ResourceId, BTreeSet<String>>,
    origins: HashMap<(ResourceId, TypeIdentity, String), String>,
}

impl ResolvedReferences {
    pub fn dangling_paths(&self, resource: &ResourceId) -> impl Iterator<Item = &str> {
        self.dangling
            .get(resource)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    fn lookup(
        &self,
        instance: &ProviderInstanceKey,
        ty: &TypeIdentity,
        raw: &str,
    ) -> Option<CurrentSide> {
        let key = CacheKey {
            instance: instance.clone(),
            ty: ty.clone(),
            raw: raw.into(),
        };
        self.desired
            .get(&key)
            .map(|DesiredSide::Canonical(value)| CurrentSide::Canonical(value.clone()))
            .or_else(|| self.current.get(&key).cloned())
    }

    pub(crate) fn equal(
        &self,
        resource: &ResourceId,
        ty: &TypeIdentity,
        a: &str,
        b: &str,
    ) -> Result<bool, Box<ReferencePlanError>> {
        if a == b {
            return Ok(true);
        }
        let instance = ProviderInstanceKey::from(resource);
        match (self.lookup(&instance, ty, a), self.lookup(&instance, ty, b)) {
            (Some(CurrentSide::Canonical(a)), Some(CurrentSide::Canonical(b))) => Ok(a == b),
            (Some(CurrentSide::Dangling), Some(_)) | (Some(_), Some(CurrentSide::Dangling)) => {
                Ok(false)
            }
            (a_value, _b_value) => {
                let raw = if a_value.is_none() { a } else { b };
                let path = self
                    .origins
                    .get(&(resource.clone(), ty.clone(), raw.into()))
                    .cloned()
                    .unwrap_or_else(|| ty.to_string());
                Err(Box::new(ReferencePlanError {
                    instance,
                    resource: resource.clone(),
                    path,
                    raw: raw.into(),
                    source: ReferenceFailure::MissingWitness,
                }))
            }
        }
    }

    pub(crate) fn validate_for(
        &self,
        desired: &[ResolvedResource],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
    ) -> Result<(), Box<ReferencePlanError>> {
        self.validate_occurrences(collect(desired, current, registry, saved))
    }

    pub(crate) fn validate_resource(
        &self,
        resource: &ResolvedResource,
        current: &crate::resource::CanonicalComparisonAttributes,
        saved: &LiftedSavedAttrs,
        schema: &crate::schema::ValidatedResourceSchema,
    ) -> Result<(), Box<ReferencePlanError>> {
        let mut occurrences = Vec::new();
        collect_resource(resource, current, saved, schema, &mut occurrences);
        self.validate_occurrences(occurrences)
    }

    fn validate_occurrences(
        &self,
        occurrences: Vec<Occurrence>,
    ) -> Result<(), Box<ReferencePlanError>> {
        for occurrence in occurrences {
            let key = occurrence.key();
            let prepared = if occurrence.desired {
                self.desired.contains_key(&key)
            } else {
                self.current.contains_key(&key) || self.desired.contains_key(&key)
            };
            if !prepared {
                return Err(occurrence.error(ReferenceFailure::MissingWitness));
            }
        }
        Ok(())
    }
}

/// A cache owned by one planning run, including its binding-stability passes.
/// A later plan creates a new run so alias changes are always observed.
#[derive(Debug, Default)]
pub struct ReferencePreparation {
    cache: HashMap<CacheKey, CachedOutcome>,
}

#[derive(Debug)]
enum CachedOutcome {
    Resolved(CanonicalReference),
    NotFound(Arc<ProviderError>),
}

impl ReferencePreparation {
    /// Prepare comparison evidence after references and provider normalization.
    /// Repeated passes in the same run only resolve newly concrete pairs.
    pub async fn prepare(
        &mut self,
        desired: &[ResolvedResource],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        provider: &ProviderRouter,
    ) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        let occurrences = collect(desired, current, registry, saved);
        let mut batches: BTreeMap<ProviderInstanceKey, Vec<&Occurrence>> = BTreeMap::new();
        for occurrence in &occurrences {
            if !self.cache.contains_key(&occurrence.key()) {
                batches
                    .entry(ProviderInstanceKey::from(&occurrence.resource))
                    .or_default()
                    .push(occurrence);
            }
        }
        for occurrences in batches.values() {
            let mut unique = HashMap::new();
            let mut requests = Vec::new();
            let mut origins = Vec::new();
            for occurrence in occurrences {
                if !unique.contains_key(&occurrence.request) {
                    unique.insert(occurrence.request.clone(), requests.len());
                    requests.push(occurrence.request.clone());
                    origins.push(*occurrence);
                }
            }
            let first = origins[0];
            let outcomes = provider
                .resolve_references_for(&first.resource, &requests)
                .await
                .map_err(|error| first.error(ReferenceFailure::Call(error)))?;
            if outcomes.len() != requests.len() {
                return Err(first.error(
                    ReferenceError::InvalidReference(ProviderError::invalid_input(format!(
                        "reference result length mismatch: expected {}, got {}",
                        requests.len(),
                        outcomes.len()
                    )))
                    .into(),
                ));
            }
            for (origin, outcome) in origins.into_iter().zip(outcomes) {
                let cached = match outcome.map_err(|error| origin.error(error.into()))? {
                    ReferenceOutcome::Resolved(value) => CachedOutcome::Resolved(value),
                    ReferenceOutcome::NotFound(error) => CachedOutcome::NotFound(Arc::new(error)),
                };
                self.cache.insert(origin.key(), cached);
            }
        }
        let mut witness = ResolvedReferences {
            desired: HashMap::new(),
            current: HashMap::new(),
            dangling: HashMap::new(),
            origins: HashMap::new(),
        };
        for occurrence in occurrences {
            let key = occurrence.key();
            witness.origins.insert(
                (
                    occurrence.resource.clone(),
                    occurrence.request.ty.clone(),
                    occurrence.request.value.clone(),
                ),
                occurrence.path.clone(),
            );
            match self
                .cache
                .get(&key)
                .expect("every needed reference is cached")
            {
                CachedOutcome::Resolved(value) => {
                    if occurrence.desired {
                        witness
                            .desired
                            .insert(key, DesiredSide::Canonical(value.clone()));
                    } else {
                        witness
                            .current
                            .insert(key, CurrentSide::Canonical(value.clone()));
                    }
                }
                CachedOutcome::NotFound(error) => {
                    if occurrence.desired {
                        return Err(
                            occurrence.error(ReferenceFailure::DesiredNotFound(Arc::clone(error)))
                        );
                    }
                    witness.current.insert(key, CurrentSide::Dangling);
                    witness
                        .dangling
                        .entry(occurrence.resource.clone())
                        .or_default()
                        .insert(occurrence.path.clone());
                }
            }
        }
        Ok(witness)
    }
}

/// Prepare an independent planning run. Hosts with several binding passes use
/// one [`ReferencePreparation`] across those passes and the final differ.
pub async fn prepare_references(
    desired: &[ResolvedResource],
    current: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved: &LiftedSavedAttrs,
    provider: &ProviderRouter,
) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
    ReferencePreparation::default()
        .prepare(desired, current, registry, saved, provider)
        .await
}

fn collect(
    desired: &[ResolvedResource],
    current: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved: &LiftedSavedAttrs,
) -> Vec<Occurrence> {
    let mut out = Vec::new();
    for resource in desired {
        let Some(current) = current.get(&resource.id) else {
            continue;
        };
        if !matches!(current.current(), Current::Present(_)) {
            continue;
        }
        let Some(schema) = registry.get_for(resource.as_resource()) else {
            continue;
        };
        collect_resource(
            resource,
            current.comparison_attributes(),
            saved,
            schema.schema(),
            &mut out,
        );
    }
    out
}

fn collect_resource(
    resource: &ResolvedResource,
    current: &crate::resource::CanonicalComparisonAttributes,
    saved: &LiftedSavedAttrs,
    schema: &crate::schema::ValidatedResourceSchema,
    out: &mut Vec<Occurrence>,
) {
    let comparison_desired = crate::resource::CanonicalComparisonAttributes::canonicalize(
        resource.resolved_attributes(),
        schema.as_schema(),
    );
    let comparison_desired = resource.ignore_changes().mask_comparison_readable_leaves(
        &comparison_desired,
        current,
        Some(schema.as_schema()),
    );
    for attribute in schema.effective_attributes() {
        let name = attribute.name();
        if resource.ignore_changes().ignores_whole_attribute(name) {
            continue;
        }
        let Some(desired) = comparison_desired.get(name) else {
            continue;
        };
        let saved_value = saved.get(&resource.id).and_then(|values| values.get(name));
        let current_value = if attribute.is_write_only() {
            saved_value
        } else {
            current.get(name)
        };
        walk(
            desired,
            current_value,
            saved_value,
            attribute.value_type(),
            resource,
            name,
            out,
            0,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    desired: &Value,
    current: Option<&Value>,
    saved: Option<&Value>,
    ty: crate::schema::EffectiveType<'_>,
    resource: &ResolvedResource,
    path: &str,
    out: &mut Vec<Occurrence>,
    depth: usize,
) {
    if depth >= 64
        || resource
            .ignore_changes()
            .iter()
            .any(|ignored| reference_path(ignored) == path)
    {
        return;
    }
    match (desired, current, ty.shape()) {
        (
            Value::Concrete(ConcreteValue::String(desired)),
            Some(Value::Concrete(ConcreteValue::String(current))),
            Shape::Reference {
                identity,
                pattern: _,
                length: _,
            },
        ) => {
            let desired = desired.expose_plaintext();
            let current = current.expose_plaintext();
            if desired == current {
                return;
            }
            for (value, desired) in [(desired, true), (current, false)] {
                out.push(Occurrence {
                    resource: resource.id.clone(),
                    path: path.into(),
                    request: ReferenceRequest {
                        ty: identity.clone(),
                        value: value.into(),
                    },
                    desired,
                });
            }
        }
        (
            Value::Concrete(ConcreteValue::Map(desired)),
            current,
            Shape::Struct { .. } | Shape::Map { .. },
        ) => {
            let current = match current {
                Some(Value::Concrete(ConcreteValue::Map(current))) => Some(current),
                None => None,
                _ => return,
            };
            let is_map = matches!(ty.shape(), Shape::Map { .. });
            let saved = match saved {
                Some(Value::Concrete(ConcreteValue::Map(saved))) => Some(saved),
                _ => None,
            };
            for (name, desired) in desired {
                let saved = saved.and_then(|values| values.get(name));
                let field = ty
                    .field_candidates()
                    .into_iter()
                    .find(|field| field.name() == name);
                let current = if field.as_ref().is_some_and(|field| field.is_write_only()) {
                    saved
                } else {
                    current.and_then(|values| values.get(name))
                };
                if let Some(ty) = field
                    .map(|field| field.value_type())
                    .or_else(|| ty.map_value())
                {
                    walk(
                        desired,
                        current,
                        saved,
                        ty,
                        resource,
                        &if is_map {
                            format!("{path}[{}]", crate::parser::display_string_literal(name))
                        } else {
                            format!("{path}.{name}")
                        },
                        out,
                        depth + 1,
                    );
                }
            }
        }
        (
            desired,
            current,
            Shape::List {
                element_type: _,
                ordered,
                length: _,
            },
        ) => {
            let Some(desired) = crate::resource::comparison_list_items(desired) else {
                return;
            };
            let current = match current {
                Some(current) => match crate::resource::comparison_list_items(current) {
                    Some(items) => Some(items),
                    None => return,
                },
                None => None,
            };
            let saved = saved.and_then(crate::resource::comparison_list_items);
            let matches = current
                .as_ref()
                .filter(|_| !ordered)
                .map(|current| crate::resource::match_equal_concrete_items(&desired, current));
            for (index, desired) in desired.iter().enumerate() {
                if matches.as_ref().is_some_and(|(a, _b)| a[index]) {
                    continue;
                }
                let candidates: Vec<_> = match &current {
                    Some(current) if ordered => current.get(index).map(Some).into_iter().collect(),
                    Some(current) => current
                        .iter()
                        .enumerate()
                        .filter(|(index, _value)| {
                            !matches.as_ref().expect("unordered matching exists").1[*index]
                        })
                        .map(|(_index, value)| Some(value))
                        .collect(),
                    None => vec![None],
                };
                for current in candidates {
                    walk(
                        desired,
                        current,
                        saved.as_ref().and_then(|values| values.get(index)),
                        ty.list_element().expect("List has element"),
                        resource,
                        &format!("{path}[{index}]"),
                        out,
                        depth + 1,
                    );
                }
            }
        }
        (Value::Deferred(_), _, _) | (_, Some(Value::Deferred(_)), _) => {}
        (_, _, Shape::Union) => {
            for member in ty.union_members() {
                walk(
                    desired,
                    current,
                    saved,
                    member,
                    resource,
                    path,
                    out,
                    depth + 1,
                );
            }
        }
        _ => {}
    }
}

fn reference_path(path: &crate::resource::ResolvedIgnorePath) -> String {
    let mut rendered = path.attribute().to_string();
    for segment in path.leaf().segments() {
        match segment {
            crate::schema::LeafPathSegment::StructField(name) => {
                rendered.push('.');
                rendered.push_str(name);
            }
            crate::schema::LeafPathSegment::ListIndex(index) => {
                rendered.push_str(&format!("[{index}]"))
            }
            crate::schema::LeafPathSegment::MapKey(key) => {
                rendered.push_str(&format!("[{}]", crate::parser::display_string_literal(key)))
            }
        }
    }
    rendered
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn prepare_empty_for_tests() -> ResolvedReferences {
    let registry = SchemaRegistry::new();
    let saved = crate::provider::RawSavedAttrs::default().lift(&registry);
    futures::executor::block_on(prepare_references(
        &[],
        &HashMap::new(),
        &registry,
        &saved,
        &ProviderRouter::new(),
    ))
    .expect("empty preparation has no provider calls")
}

#[cfg(test)]
mod tests;
