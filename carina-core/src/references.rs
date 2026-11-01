//! Comparison-only, guest-resolved reference identities. Raw values never change.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use crate::provider::{LiftedSavedAttrs, ProviderError, ProviderInstanceKey, ProviderRouter};
use crate::resource::{
    ConcreteValue, Current, PlanInputCurrent, ResolvedResource, ResourceId, Value, ValueString,
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
    pub raw: ValueString,
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
            raw: self.request.value.clone().into(),
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
    comparisons: HashMap<ResourceId, PreparedComparison>,
}

/// Keep the exact masked view used by the collector for the planning walkers.
/// Originals are only a reuse guard: binding-stability passes may change a
/// resource's concrete inputs without changing its routed identity.
#[derive(Debug, Clone)]
struct PreparedComparison {
    desired: crate::resource::CanonicalComparisonAttributes,
    current: crate::resource::CanonicalComparisonAttributes,
    saved: Option<crate::resource::CanonicalComparisonAttributes>,
    ignores: crate::resource::ResolvedIgnoreChanges,
    masked: crate::resource::MaskedComparisonAttributes,
}

struct ReferenceInputs {
    occurrences: Vec<Occurrence>,
    comparisons: HashMap<ResourceId, PreparedComparison>,
}

impl ResolvedReferences {
    pub(crate) fn comparison_attributes(
        &self,
        resource: &ResourceId,
        desired: &crate::resource::CanonicalComparisonAttributes,
        current: &crate::resource::CanonicalComparisonAttributes,
        saved: Option<&crate::resource::CanonicalComparisonAttributes>,
        ignores: &crate::resource::ResolvedIgnoreChanges,
    ) -> Option<&crate::resource::MaskedComparisonAttributes> {
        let prepared = self.comparisons.get(resource)?;
        (prepared.desired == *desired
            && prepared.current == *current
            && prepared.saved.as_ref() == saved
            && prepared.ignores == *ignores)
            .then_some(&prepared.masked)
    }
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
        path: &str,
        ty: &TypeIdentity,
        a: &ValueString,
        b: &ValueString,
    ) -> Result<bool, Box<ReferencePlanError>> {
        if let Some(raw) = [a, b].into_iter().find(|raw| raw.is_diagnostic_sensitive()) {
            return Err(Box::new(ReferencePlanError {
                instance: ProviderInstanceKey::from(resource),
                resource: resource.clone(),
                path: path.into(),
                raw: raw.clone(),
                source: ReferenceError::InvalidReference(ProviderError::invalid_input(
                    "secret values cannot be used as references",
                ))
                .into(),
            }));
        }
        let a = a.expose_plaintext();
        let b = b.expose_plaintext();
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
                Err(Box::new(ReferencePlanError {
                    instance,
                    resource: resource.clone(),
                    path: path.into(),
                    raw: raw.into(),
                    source: ReferenceFailure::MissingWitness,
                }))
            }
        }
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
    pub async fn prepare<S>(
        &mut self,
        desired: &[ResolvedResource<S>],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        provider: &ProviderRouter,
    ) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        let inputs = collect(desired, current, registry, saved)?;
        let mut references = self
            .prepare_occurrences(inputs.occurrences, provider)
            .await?;
        references.comparisons = inputs.comparisons;
        Ok(references)
    }

    /// Apply-time decisions use exactly the same masked operands for
    /// preparation and comparison, without sharing the plan's stale cache.
    pub(crate) async fn prepare_masked_resource(
        &mut self,
        resource: &ResourceId,
        masked: &crate::resource::MaskedComparisonAttributes,
        schema: &crate::schema::ValidatedResourceSchema,
        provider: &ProviderRouter,
    ) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        let mut collector = ReferenceCollector {
            resource,
            paths: HashMap::new(),
        };
        collector.collect(masked, schema)?;
        self.prepare_occurrences(collector.needed_occurrences(), provider)
            .await
    }

    async fn prepare_occurrences(
        &mut self,
        occurrences: Vec<Occurrence>,
        provider: &ProviderRouter,
    ) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        let mut batches: BTreeMap<ProviderInstanceKey, Vec<&Occurrence>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for occurrence in &occurrences {
            let key = occurrence.key();
            if !self.cache.contains_key(&key) && seen.insert(key) {
                batches
                    .entry(ProviderInstanceKey::from(&occurrence.resource))
                    .or_default()
                    .push(occurrence);
            }
        }
        let batches =
            futures::future::join_all(batches.into_values().map(|occurrences| async move {
                let requests = occurrences
                    .iter()
                    .map(|occurrence| occurrence.request.clone())
                    .collect::<Vec<_>>();
                let first = occurrences[0];
                let outcomes = provider
                    .resolve_references_for(&first.resource, &requests)
                    .await
                    .map_err(|error| first.error(ReferenceFailure::Call(error)))?;
                if outcomes.len() != requests.len() {
                    return Err(first.error(
                        ReferenceError::InvalidReference(ProviderError::invalid_input(format!(
                            "reference result length mismatch: expected {}, got {}",
                            requests.len(),
                            outcomes.len(),
                        )))
                        .into(),
                    ));
                }
                Ok::<_, Box<ReferencePlanError>>(
                    occurrences.into_iter().zip(outcomes).collect::<Vec<_>>(),
                )
            }))
            .await;
        for batch in batches {
            for (origin, outcome) in batch? {
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
            comparisons: HashMap::new(),
        };
        for occurrence in occurrences {
            let key = occurrence.key();
            match self
                .cache
                .get(&key)
                .ok_or_else(|| occurrence.error(ReferenceFailure::MissingWitness))?
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

fn collect<S>(
    desired: &[ResolvedResource<S>],
    current: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved: &LiftedSavedAttrs,
) -> Result<ReferenceInputs, Box<ReferencePlanError>> {
    let mut out = Vec::new();
    let mut comparisons = HashMap::new();
    for resource in desired {
        let Some(schema) = registry.get_for(resource.as_resource()) else {
            continue;
        };
        let current = current
            .get(&resource.id)
            .filter(|current| matches!(current.current(), Current::Present(_)));
        let empty = crate::resource::CanonicalComparisonAttributes::default();
        let current_attributes = current.map_or(&empty, PlanInputCurrent::comparison_attributes);
        let comparison_desired = crate::resource::CanonicalComparisonAttributes::canonicalize(
            resource.resolved_attributes(),
            schema.schema().as_schema(),
        );
        let comparison_saved = saved.get(&resource.id).map(|values| {
            crate::resource::CanonicalComparisonAttributes::canonicalize(
                values.clone(),
                schema.schema().as_schema(),
            )
        });
        let masked = resource.ignore_changes().mask_comparison_attributes(
            &comparison_desired,
            current_attributes,
            comparison_saved.as_ref(),
            Some(schema.schema().as_schema()),
        );
        let mut collector = ReferenceCollector {
            resource: &resource.id,
            paths: HashMap::new(),
        };
        collector.collect(&masked, schema.schema())?;
        comparisons.insert(
            resource.id.clone(),
            PreparedComparison {
                desired: comparison_desired,
                current: current_attributes.clone(),
                saved: comparison_saved,
                ignores: resource.ignore_changes().clone(),
                masked,
            },
        );
        // Creates have no prior comparison. Walking their desired values still
        // rejects secrets before any provider batch can be dispatched.
        if current.is_some() {
            out.extend(collector.needed_occurrences());
        }
    }
    Ok(ReferenceInputs {
        occurrences: out,
        comparisons,
    })
}

/// Gather all operands a structural comparison can pair. List matching and
/// similarity scoring can compare any elements, including desired vs saved;
/// element indices therefore belong to diagnostics, not the schema path key.
struct ReferenceCollector<'a> {
    resource: &'a ResourceId,
    paths: HashMap<(String, TypeIdentity), Vec<Occurrence>>,
}

impl ReferenceCollector<'_> {
    fn needed_occurrences(self) -> Vec<Occurrence> {
        self.paths
            .into_values()
            .filter(|occurrences| {
                occurrences
                    .iter()
                    .map(|occurrence| &occurrence.request.value)
                    .collect::<HashSet<_>>()
                    .len()
                    > 1
            })
            .flatten()
            .collect()
    }
    fn collect(
        &mut self,
        masked: &crate::resource::MaskedComparisonAttributes,
        schema: &crate::schema::ValidatedResourceSchema,
    ) -> Result<(), Box<ReferencePlanError>> {
        for attribute in schema.effective_attributes() {
            let name = attribute.name();
            for (value, desired) in [
                (masked.desired().get(name), true),
                (masked.current().get(name), false),
                (masked.saved().and_then(|values| values.get(name)), false),
            ] {
                if let Some(value) = value {
                    self.walk(value, attribute.value_type(), name, name, desired, false, 0)?;
                }
            }
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        value: &Value,
        ty: crate::schema::EffectiveType<'_>,
        path: &str,
        schema_path: &str,
        desired: bool,
        secret: bool,
        depth: usize,
    ) -> Result<(), Box<ReferencePlanError>> {
        if depth >= 64 {
            return Ok(());
        }
        if let Value::Deferred(crate::resource::DeferredValue::Secret(inner)) = value {
            return self.walk(inner, ty, path, schema_path, desired, true, depth + 1);
        }
        match ty.shape() {
            Shape::Reference { identity, .. } => {
                if secret
                    || crate::value::contains_secret(value)
                    || matches!(value, Value::Concrete(ConcreteValue::String(raw)) if raw.is_diagnostic_sensitive())
                {
                    return Err(Box::new(ReferencePlanError {
                        instance: ProviderInstanceKey::from(self.resource),
                        resource: self.resource.clone(),
                        path: path.into(),
                        raw: ValueString::diagnostic_sensitive("<redacted>"),
                        source: ReferenceError::InvalidReference(ProviderError::invalid_input(
                            "secret values cannot be used as references",
                        ))
                        .into(),
                    }));
                }
                if let Value::Concrete(ConcreteValue::String(raw)) = value {
                    self.paths
                        .entry((schema_path.into(), identity.clone()))
                        .or_default()
                        .push(Occurrence {
                            resource: self.resource.clone(),
                            path: path.into(),
                            desired,
                            request: ReferenceRequest {
                                ty: identity.clone(),
                                value: raw.expose_plaintext().into(),
                            },
                        });
                }
            }
            Shape::Struct { .. } | Shape::Map { .. } => {
                if let Value::Concrete(ConcreteValue::Map(values)) = value {
                    let is_map = matches!(ty.shape(), Shape::Map { .. });
                    for (name, value) in values {
                        let field = ty
                            .field_candidates()
                            .into_iter()
                            .find(|field| field.name() == name);
                        if let Some(child_type) = field
                            .map(|field| field.value_type())
                            .or_else(|| ty.map_value())
                        {
                            let (path, schema_path) = if is_map {
                                (
                                    format!(
                                        "{path}[{}]",
                                        crate::parser::display_string_literal(name)
                                    ),
                                    format!("{schema_path}.*"),
                                )
                            } else {
                                (format!("{path}.{name}"), format!("{schema_path}.{name}"))
                            };
                            self.walk(
                                value,
                                child_type,
                                &path,
                                &schema_path,
                                desired,
                                secret,
                                depth + 1,
                            )?;
                        }
                    }
                }
            }
            Shape::List { .. } => {
                if let Some(items) = crate::resource::comparison_list_items(value)
                    && let Some(element) = ty.list_element()
                {
                    for (index, value) in items.iter().enumerate() {
                        self.walk(
                            value,
                            element.clone(),
                            &format!("{path}[{index}]"),
                            &format!("{schema_path}[]"),
                            desired,
                            secret,
                            depth + 1,
                        )?;
                    }
                }
            }
            Shape::Union => {
                for member in ty.union_members() {
                    self.walk(value, member, path, schema_path, desired, secret, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn prepare_empty_for_tests() -> ResolvedReferences {
    ResolvedReferences {
        desired: HashMap::new(),
        current: HashMap::new(),
        dangling: HashMap::new(),
        comparisons: HashMap::new(),
    }
}

#[cfg(test)]
mod tests;
