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

#[derive(Debug, Clone, thiserror::Error)]
pub enum ReferenceError {
    #[error("reference access denied")] // error-chain: caller-renders-sources
    AccessDenied(#[source] Arc<ProviderError>),
    #[error("reference resolution temporarily failed")] // error-chain: caller-renders-sources
    Transient(#[source] Arc<ProviderError>),
    #[error("invalid reference")] // error-chain: caller-renders-sources
    InvalidReference(#[source] Arc<ProviderError>),
}

impl ReferenceOutcome {
    pub fn from_guest(
        outcome: carina_provider_protocol::types::ReferenceOutcome,
    ) -> Result<Self, ReferenceError> {
        use carina_provider_protocol::types::ReferenceOutcome as Guest;
        match outcome {
            Guest::Resolved(value) if value.is_empty() => Err(ReferenceError::InvalidReference(
                ProviderError::invalid_input("guest returned an empty canonical reference").into(),
            )),
            // The guest owns canonical format validation (for example, ARN
            // shape). Core/host validate only non-emptiness and batch length;
            // interpreting an AWS ARN here would make this seam provider-specific.
            Guest::Resolved(value) => Ok(Self::Resolved(CanonicalReference(value))),
            Guest::NotFound(error) => Ok(Self::NotFound(ProviderError::from_wire(*error))),
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ReferenceFailure {
    #[error("desired reference does not resolve")] // error-chain: caller-renders-sources
    DesiredNotFound(#[source] Arc<ProviderError>),
    #[error(transparent)]
    Resolution(#[from] ReferenceError),
    #[error("provider reference resolution call failed")] // error-chain: caller-renders-sources
    Call(#[source] Arc<ProviderError>),
    #[error("reference comparison was not prepared")]
    MissingWitness,
    #[error("internal error: reference comparison was not prepared after the single replay")]
    UnpreparedAfterReplay,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[doc(hidden)]
pub enum ReferenceSide {
    Desired,
    Current,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DanglingReference {
    pub(crate) path: String,
    pub(crate) side: ReferenceSide,
}

impl ReferenceSide {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Desired => "desired",
            Self::Current => "current",
            Self::Saved => "saved",
        }
    }
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

/// Evidence produced only by a recording-free computation. The recording
/// variant is private to `ReferencePreparation::run` and never reaches callers.
///
/// ```compile_fail,E0599
/// use carina_core::references::ResolvedReferences;
/// let forged = ResolvedReferences::default();
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedReferences {
    values: HashMap<CacheKey, CachedOutcome>,
    dangling: HashMap<ResourceId, BTreeSet<DanglingReference>>,
    recording: Option<Arc<std::sync::Mutex<Recording>>>,
}

#[derive(Debug, Default)]
struct Recording {
    missing: Vec<Occurrence>,
    dangling: HashMap<ResourceId, BTreeSet<DanglingReference>>,
}

/// The actual operand origin, carried through structural comparisons, list
/// pairing and saved-value merges. It is independent of a value's bytes.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct ReferenceOperand {
    pub(crate) path: String,
    pub(crate) side: ReferenceSide,
}

impl ResolvedReferences {
    fn first_missing(&self) -> Option<Occurrence> {
        self.recording
            .as_ref()
            .expect("recording computation")
            .lock()
            .expect("reference recording lock")
            .missing
            .first()
            .cloned()
    }

    /// The sole release boundary, used only after checking the recording.
    fn release<T, E>(mut self, value: Result<T, E>) -> Result<PreparedComparison<T>, E> {
        let value = value?;
        self.dangling = self
            .recording
            .take()
            .expect("recording computation")
            .lock()
            .expect("reference recording lock")
            .dangling
            .clone();
        Ok(PreparedComparison {
            value,
            references: self,
        })
    }

    #[cfg(test)]
    fn dangling_paths(&self, resource: &ResourceId) -> impl Iterator<Item = String> {
        let mut seen = HashSet::new();
        self.dangling_references(resource)
            .map(|reference| reference.path)
            .filter(move |path| seen.insert(path.clone()))
    }

    pub(crate) fn dangling_references(
        &self,
        resource: &ResourceId,
    ) -> impl Iterator<Item = DanglingReference> {
        let mut dangling = self.dangling.get(resource).cloned().unwrap_or_default();
        if let Some(recording) = &self.recording {
            dangling.extend(
                recording
                    .lock()
                    .expect("reference recording lock")
                    .dangling
                    .get(resource)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        dangling.into_iter()
    }

    pub(crate) fn equal(
        &self,
        resource: &ResourceId,
        ty: &TypeIdentity,
        a: &ValueString,
        b: &ValueString,
        operands: &[ReferenceOperand; 2],
    ) -> Result<bool, Box<ReferencePlanError>> {
        for (raw, operand) in [a, b].into_iter().zip(operands) {
            if raw.is_diagnostic_sensitive() {
                return Err(Box::new(ReferencePlanError {
                    instance: ProviderInstanceKey::from(resource),
                    resource: resource.clone(),
                    path: operand.path.clone(),
                    raw: raw.clone(),
                    source: ReferenceError::InvalidReference(
                        ProviderError::invalid_input("secret values cannot be used as references")
                            .into(),
                    )
                    .into(),
                }));
            }
        }
        if a == b {
            return Ok(true);
        }
        let mut canonical = Vec::with_capacity(2);
        for (raw, operand) in [a, b].into_iter().zip(operands) {
            let occurrence = Occurrence {
                resource: resource.clone(),
                path: operand.path.clone(),
                request: ReferenceRequest {
                    ty: ty.clone(),
                    value: raw.expose_plaintext().into(),
                },
            };
            match self.values.get(&occurrence.key()) {
                Some(CachedOutcome::Resolved(value)) => canonical.push(Some(value)),
                Some(CachedOutcome::Failed(error)) => return Err(occurrence.error(error.clone())),
                Some(CachedOutcome::NotFound(error)) => {
                    if operand.side == ReferenceSide::Desired {
                        return Err(
                            occurrence.error(ReferenceFailure::DesiredNotFound(Arc::clone(error)))
                        );
                    }
                    if let Some(recording) = &self.recording {
                        recording
                            .lock()
                            .expect("reference recording lock")
                            .dangling
                            .entry(resource.clone())
                            .or_default()
                            .insert(DanglingReference {
                                path: operand.path.clone(),
                                side: operand.side,
                            });
                    }
                    canonical.push(None);
                }
                None => {
                    let Some(recording) = &self.recording else {
                        return Err(occurrence.error(ReferenceFailure::MissingWitness));
                    };
                    recording
                        .lock()
                        .expect("reference recording lock")
                        .missing
                        .push(occurrence);
                    canonical.push(None);
                }
            }
        }
        // Missing operands provisionally compare unequal. No result of such
        // a pass can cross the ProvisionalComparison release boundary.
        Ok(matches!((&canonical[0], &canonical[1]), (Some(a), Some(b)) if a == b))
    }
}

/// Only `run` can release this result, after checking the entire recording.
/// A recording session cannot expose its witness or any provisional output.
///
/// ```compile_fail,E0616
/// use carina_core::{references::ProvisionalComparison, plan::Plan};
/// fn escape(result: ProvisionalComparison<'_, Result<Plan, ()>>) -> Plan {
///     result.value.unwrap()
/// }
/// ```
///
/// ```compile_fail,E0616
/// use carina_core::references::RecordingSession;
/// fn escape(session: RecordingSession<'_>) { let witness = session.references; }
/// ```
#[must_use]
pub struct ProvisionalComparison<'run, T> {
    value: T,
    _session: std::marker::PhantomData<&'run ()>,
}

/// Capabilities for running the real planning computations. Each operation
/// returns an opaque provisional result; there is no public generic callback
/// with access to recording evidence or to provisional plans/rows.
#[derive(Clone, Copy)]
pub struct RecordingSession<'run> {
    references: &'run ResolvedReferences,
}

/// A computation and its witness, released together after a clean pass.
#[derive(Debug)]
pub struct PreparedComparison<T> {
    pub value: T,
    pub references: ResolvedReferences,
}

/// One planning invocation, including binding-stability passes. A new
/// invocation starts empty so changes to external aliases are observed.
#[derive(Debug, Default)]
pub struct ReferencePreparation {
    cache: HashMap<CacheKey, CachedOutcome>,
}

#[derive(Debug, Clone)]
enum CachedOutcome {
    Resolved(CanonicalReference),
    NotFound(Arc<ProviderError>),
    Failed(ReferenceFailure),
}

impl ReferencePreparation {
    fn recording(&self) -> ResolvedReferences {
        ResolvedReferences {
            values: self.cache.clone(),
            recording: Some(Arc::default()),
            dangling: HashMap::new(),
        }
    }

    async fn run<T, E>(
        &mut self,
        provider: &ProviderRouter,
        enumerate: impl FnOnce() -> Result<Vec<Occurrence>, Box<ReferencePlanError>>,
        mut compute: impl for<'run> FnMut(
            RecordingSession<'run>,
        ) -> ProvisionalComparison<'run, Result<T, E>>,
    ) -> Result<PreparedComparison<T>, E>
    where
        E: From<Box<ReferencePlanError>>,
    {
        let references = self.recording();
        let provisional = compute(RecordingSession {
            references: &references,
        });
        if references.first_missing().is_none() {
            let value = provisional.value;
            return references.release(value);
        }
        // Discard even provisional errors: identities can change control flow.
        drop(provisional);
        let occurrences = enumerate()?;
        self.resolve_occurrences(&occurrences, provider).await;

        let references = self.recording();
        let replay = compute(RecordingSession {
            references: &references,
        });
        if let Some(missing) = references.first_missing() {
            return Err(missing
                .error(ReferenceFailure::UnpreparedAfterReplay)
                .into());
        }
        let value = replay.value;
        references.release(value)
    }

    async fn resolve_occurrences(&mut self, occurrences: &[Occurrence], provider: &ProviderRouter) {
        let mut batches: BTreeMap<ProviderInstanceKey, Vec<&Occurrence>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for occurrence in occurrences {
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
                    .map(|origin| origin.request.clone())
                    .collect::<Vec<_>>();
                let first = occurrences[0];
                let outcomes = provider
                    .resolve_references_for(&first.resource, &requests)
                    .await;
                let outcomes = match outcomes {
                    Err(error) => vec![
                        CachedOutcome::Failed(ReferenceFailure::Call(error.into()));
                        requests.len()
                    ],
                    Ok(outcomes) if outcomes.len() != requests.len() => {
                        let error = ReferenceError::InvalidReference(
                            ProviderError::invalid_input(format!(
                                "reference result length mismatch: expected {}, got {}",
                                requests.len(),
                                outcomes.len(),
                            ))
                            .into(),
                        )
                        .into();
                        vec![CachedOutcome::Failed(error); requests.len()]
                    }
                    Ok(outcomes) => outcomes
                        .into_iter()
                        .map(|outcome| match outcome {
                            Ok(ReferenceOutcome::Resolved(value)) => CachedOutcome::Resolved(value),
                            Ok(ReferenceOutcome::NotFound(error)) => {
                                CachedOutcome::NotFound(Arc::new(error))
                            }
                            Err(error) => CachedOutcome::Failed(error.into()),
                        })
                        .collect(),
                };
                occurrences.into_iter().zip(outcomes).collect::<Vec<_>>()
            }))
            .await;
        for batch in batches {
            for (origin, outcome) in batch {
                self.cache.insert(origin.key(), outcome);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stability(
        &mut self,
        desired: &[ResolvedResource],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        provider: &ProviderRouter,
    ) -> crate::differ::PlanComparisonResult<
        PreparedComparison<HashMap<ResourceId, crate::binding_index::ManagedBindingStability>>,
    > {
        validate_inputs(desired, current, registry, saved)?;
        self.run(
            provider,
            || collect_inputs(desired, current, registry, saved),
            |session| {
                session.result(crate::differ::managed_binding_stability(
                    desired,
                    current,
                    registry,
                    saved,
                    explicit,
                    provider,
                    session.references,
                ))
            },
        )
        .await
    }

    /// Prepare the differ and cascade decisions. Finalize ancillary effects
    /// before calling `display` with the complete historical display view.
    #[allow(clippy::too_many_arguments)]
    pub async fn plan_with_cascades(
        &mut self,
        desired: &crate::resource::PlanInputDesiredResources<'_>,
        dependents: &crate::differ::CascadeDependents<'_>,
        data_sources: &[crate::resource::DataSource],
        provider: &ProviderRouter,
        prior: crate::differ::PlanPriorStates<'_>,
        directives: &HashMap<ResourceId, crate::resource::Directives>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        waits: &[crate::parser::WaitBinding<crate::resource::AbsoluteBindingAddress>],
        unknown_imports: &HashSet<crate::effect::OutcomeUnknownImportReplacement>,
    ) -> crate::differ::PlanComparisonResult<PreparedComparison<crate::differ::PendingCascadePlan>>
    {
        let current = prior.materialize_current_states(desired.authored(), registry);
        validate_inputs(desired.authored(), &current, registry, saved)?;
        validate_inputs(dependents.managed(), &current, registry, saved)?;
        self.run(
            provider,
            || {
                let mut values = collect_inputs(desired.authored(), &current, registry, saved)?;
                values.extend(collect_inputs(
                    dependents.managed(),
                    &current,
                    registry,
                    saved,
                )?);
                Ok(values)
            },
            |session| {
                session.result(crate::differ::create_plan_with_cascades(
                    desired,
                    dependents.for_recording_run(),
                    data_sources,
                    provider,
                    prior,
                    directives,
                    registry,
                    saved,
                    explicit,
                    waits,
                    unknown_imports,
                    session.references,
                ))
            },
        )
        .await
    }

    /// Consume the finalized plan and render only the requested detail level.
    /// Each recording pass constructs its rows together with that immutable
    /// plan; only a clean pass can release the owning display value.
    #[allow(clippy::too_many_arguments)]
    pub async fn display(
        &mut self,
        plan: crate::plan::Plan,
        planning: &ResolvedReferences,
        historical: crate::detail_rows::HistoricalDisplayAttributes,
        detail: crate::detail_rows::DetailLevel,
        registry: &SchemaRegistry,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        provider: &ProviderRouter,
    ) -> crate::provider::ProviderResult<PreparedComparison<crate::detail_rows::PreparedPlanDisplay>>
    {
        let plan = Arc::new(plan);
        self.run(
            provider,
            || {
                let mut values = Vec::new();
                for effect in plan.effects() {
                    match effect {
                        crate::effect::Effect::Create(resource)
                        | crate::effect::Effect::Update { to: resource, .. } => {
                            if let Some(schema) = registry.get_for(resource) {
                                collect_attributes(
                                    &resource.id,
                                    resource.resolved_attributes().iter(),
                                    schema.schema(),
                                    &mut values,
                                )?;
                            }
                        }
                        _ => {}
                    }
                }
                for (id, attributes) in historical.reference_values() {
                    if let Some(schema) = registry.get(
                        &id.provider,
                        &id.resource_type,
                        crate::schema::SchemaKind::Resource,
                    ) {
                        collect_attributes(id, attributes.iter(), schema.schema(), &mut values)?;
                    }
                }
                for (id, record) in historical.reference_records() {
                    if let Some(schema) = registry.get(
                        &id.provider,
                        &id.resource_type,
                        crate::schema::SchemaKind::Resource,
                    ) {
                        collect_record(id, record, schema.schema(), &mut values)?;
                    }
                }
                Ok(values)
            },
            |session| {
                session.render(
                    Arc::clone(&plan),
                    planning,
                    historical.clone(),
                    detail,
                    registry,
                    explicit,
                    provider,
                )
            },
        )
        .await
    }

    /// Test compatibility seam for callers without CLI-owned state history.
    #[cfg(any(test, feature = "test-utils"))]
    #[allow(clippy::too_many_arguments)]
    pub async fn plan(
        &mut self,
        desired: &[ResolvedResource],
        data_sources: &[crate::resource::DataSource],
        provider: &ProviderRouter,
        prior: crate::differ::PlanPriorStates<'_>,
        directives: &HashMap<ResourceId, crate::resource::Directives>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        waits: &[crate::parser::WaitBinding<crate::resource::AbsoluteBindingAddress>],
    ) -> crate::differ::PlanComparisonResult<PreparedComparison<crate::plan::Plan>> {
        validate_inputs(
            desired,
            &prior.materialize_current_states(desired, registry),
            registry,
            saved,
        )?;
        let prepared = self
            .run(
                provider,
                || {
                    collect_inputs(
                        desired,
                        &prior.materialize_current_states(desired, registry),
                        registry,
                        saved,
                    )
                },
                |session| {
                    session.result(crate::differ::create_plan(
                        desired,
                        data_sources,
                        provider,
                        prior,
                        directives,
                        registry,
                        saved,
                        explicit,
                        waits,
                        session.references,
                    ))
                },
            )
            .await?;
        let plan = prepared.value;
        let historical =
            crate::detail_rows::HistoricalDisplayAttributes::for_planning(&plan, saved);
        let display = self
            .display(
                plan.clone(),
                &prepared.references,
                historical,
                crate::detail_rows::DetailLevel::Full,
                registry,
                explicit,
                provider,
            )
            .await?;
        Ok(PreparedComparison {
            value: plan,
            references: display.references,
        })
    }

    /// Focused compatibility helper. Production prepares cascade decisions and
    /// then display rows using the finalized plan and its state history.
    #[cfg(any(test, feature = "test-utils"))]
    pub async fn prepare(
        &mut self,
        desired: &[ResolvedResource],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        provider: &ProviderRouter,
    ) -> Result<ResolvedReferences, Box<ReferencePlanError>> {
        let result = self
            .plan(
                desired,
                &[],
                provider,
                crate::differ::PlanPriorStates::new(current, &crate::differ::Orphans::new()),
                &HashMap::new(),
                registry,
                saved,
                &HashMap::new(),
                &[],
            )
            .await;
        match result {
            Ok(prepared) => Ok(prepared.references),
            Err(crate::differ::PlanComparisonError::Reference(error)) => Err(error),
            Err(crate::differ::PlanComparisonError::Provider(error)) => {
                Err(Box::new(ReferencePlanError {
                    instance: ProviderInstanceKey::from(&desired[0].id),
                    resource: desired[0].id.clone(),
                    path: "<planning>".into(),
                    raw: "<planning>".into(),
                    source: ReferenceFailure::Call(error.into()),
                }))
            }
        }
    }

    pub(crate) async fn apply_time<S>(
        &mut self,
        resource: &ResolvedResource<S>,
        masked: &crate::resource::MaskedComparisonAttributes,
        schema: crate::schema::OwnerBoundResourceSchema<'_>,
        provider: &ProviderRouter,
    ) -> Result<PreparedComparison<Vec<String>>, crate::differ::ApplyTimeComparisonError> {
        self.run(
            provider,
            || {
                let mut values = Vec::new();
                for attributes in [
                    Some(masked.desired()),
                    Some(masked.current()),
                    masked.saved(),
                ]
                .into_iter()
                .flatten()
                {
                    collect_attributes(
                        &resource.id,
                        attributes.iter(),
                        schema.schema(),
                        &mut values,
                    )?;
                }
                Ok(values)
            },
            |session| session.apply_time(resource, masked, schema, provider),
        )
        .await
    }
}

impl<'run> RecordingSession<'run> {
    fn result<T>(self, value: T) -> ProvisionalComparison<'run, T> {
        ProvisionalComparison {
            value,
            _session: std::marker::PhantomData,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render(
        self,
        plan: Arc<crate::plan::Plan>,
        planning: &ResolvedReferences,
        historical: crate::detail_rows::HistoricalDisplayAttributes,
        detail: crate::detail_rows::DetailLevel,
        registry: &SchemaRegistry,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        provider: &ProviderRouter,
    ) -> ProvisionalComparison<
        'run,
        crate::provider::ProviderResult<crate::detail_rows::PreparedPlanDisplay>,
    > {
        self.references
            .recording
            .as_ref()
            .expect("recording pass")
            .lock()
            .expect("reference recording lock")
            .dangling = planning.dangling.clone();
        self.result(crate::detail_rows::PreparedPlanDisplay::build(
            plan,
            historical,
            detail,
            registry,
            explicit,
            provider,
            self.references,
        ))
    }

    pub(crate) fn apply_time<S>(
        self,
        resource: &ResolvedResource<S>,
        masked: &crate::resource::MaskedComparisonAttributes,
        schema: crate::schema::OwnerBoundResourceSchema<'_>,
        provider: &ProviderRouter,
    ) -> ProvisionalComparison<'run, Result<Vec<String>, crate::differ::ApplyTimeComparisonError>>
    {
        self.result((|| {
            ReferenceValidation {
                resource: &resource.id,
            }
            .masked(masked, schema.schema())?;
            let comparison = provider
                .comparison_context_for(&resource.id, schema)
                .map_err(|error| {
                    Box::new(ReferencePlanError {
                        instance: ProviderInstanceKey::from(&resource.id),
                        resource: resource.id.clone(),
                        path: "<apply>".into(),
                        raw: "<apply>".into(),
                        source: ReferenceFailure::Call(error.into()),
                    })
                })?;
            Ok(
                comparison.with_references(self.references, &resource.id, |context| {
                    crate::differ::find_apply_time_changed_attributes(
                        masked,
                        resource,
                        schema.schema().as_schema(),
                        resource.ignore_changes(),
                        context,
                    )
                })??,
            )
        })())
    }
}

/// Check traversal depth before any guest call, independently of comparison
/// order. Secret errors are decided only when a comparison uses an operand.
fn validate_inputs(
    desired: &[ResolvedResource],
    current: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved: &LiftedSavedAttrs,
) -> Result<(), Box<ReferencePlanError>> {
    for resource in desired {
        let Some(schema) = registry.get_for(resource.as_resource()) else {
            continue;
        };
        let desired = crate::resource::CanonicalComparisonAttributes::canonicalize(
            resource.resolved_attributes(),
            schema.schema().as_schema(),
        );
        let empty = crate::resource::CanonicalComparisonAttributes::default();
        let current = current
            .get(&resource.id)
            .filter(|value| matches!(value.current(), Current::Present(_)))
            .map_or(&empty, PlanInputCurrent::comparison_attributes);
        let saved = saved.get(&resource.id).map(|values| {
            crate::resource::CanonicalComparisonAttributes::canonicalize(
                values.clone(),
                schema.schema().as_schema(),
            )
        });
        let masked = resource.ignore_changes().mask_comparison_attributes(
            &desired,
            current,
            saved.as_ref(),
            Some(schema.schema().as_schema()),
        );
        ReferenceValidation {
            resource: &resource.id,
        }
        .masked(&masked, schema.schema())?;
    }
    Ok(())
}

/// Enumerate values independently of comparison order, projection and pairing.
/// A replay can therefore take a different branch without another lookup round.
fn collect_inputs(
    desired: &[ResolvedResource],
    current: &HashMap<ResourceId, PlanInputCurrent>,
    registry: &SchemaRegistry,
    saved: &LiftedSavedAttrs,
) -> Result<Vec<Occurrence>, Box<ReferencePlanError>> {
    let mut values = Vec::new();
    for resource in desired {
        if let Some(schema) = registry.get_for(resource.as_resource()) {
            collect_attributes(
                &resource.id,
                resource.resolved_attributes().iter(),
                schema.schema(),
                &mut values,
            )?;
        }
    }
    for (id, current) in current {
        if let Some(schema) = registry.get(
            &id.provider,
            &id.resource_type,
            crate::schema::SchemaKind::Resource,
        ) {
            collect_attributes(
                id,
                current.comparison_attributes().iter(),
                schema.schema(),
                &mut values,
            )?;
        }
    }
    for id in saved.resource_ids() {
        if let Some(schema) = registry.get(
            &id.provider,
            &id.resource_type,
            crate::schema::SchemaKind::Resource,
        ) {
            collect_attributes(
                id,
                saved.get(id).expect("saved resource").iter(),
                schema.schema(),
                &mut values,
            )?;
        }
    }
    let resources = desired
        .iter()
        .map(|resource| &resource.id)
        .chain(current.keys())
        .chain(saved.resource_ids())
        .collect::<HashSet<_>>();
    for id in resources {
        if let Some(schema) = registry.get(
            &id.provider,
            &id.resource_type,
            crate::schema::SchemaKind::Resource,
        ) {
            collect_record(
                id,
                saved.nested_write_only(id),
                schema.schema(),
                &mut values,
            )?;
        }
    }
    Ok(values)
}

fn collect_attributes<'a>(
    resource: &ResourceId,
    attributes: impl Iterator<Item = (&'a String, &'a Value)>,
    schema: &crate::schema::ValidatedResourceSchema,
    values: &mut Vec<Occurrence>,
) -> Result<(), Box<ReferencePlanError>> {
    for (name, value) in attributes {
        if let Some(attribute) = schema.effective_attribute(name) {
            ReferenceValidation { resource }.walk(
                value,
                attribute.value_type(),
                name,
                false,
                0,
                &mut |value, ty, path, secret| {
                    collect_leaf(resource, value, ty, path, secret, values)
                },
            )?;
        }
    }
    Ok(())
}

fn collect_leaf(
    resource: &ResourceId,
    value: &Value,
    ty: &TypeIdentity,
    path: &str,
    secret: bool,
    values: &mut Vec<Occurrence>,
) -> Result<(), Box<ReferencePlanError>> {
    // Secrets are never sent to a guest. Only a comparison that uses one
    // returns its typed error.
    if !secret
        && let Value::Concrete(ConcreteValue::String(raw)) = value
        && !raw.is_diagnostic_sensitive()
    {
        values.push(Occurrence {
            resource: resource.clone(),
            path: path.into(),
            request: ReferenceRequest {
                ty: ty.clone(),
                value: raw.expose_plaintext().into(),
            },
        });
    }
    Ok(())
}

fn collect_record(
    resource: &ResourceId,
    record: &crate::provider::NestedWriteOnlyRecord,
    schema: &crate::schema::ValidatedResourceSchema,
    values: &mut Vec<Occurrence>,
) -> Result<(), Box<ReferencePlanError>> {
    for attribute in schema.effective_attributes() {
        for (path, persisted) in record.get(attribute.name()).into_iter().flatten() {
            if let Some(value) = persisted.decode() {
                ReferenceValidation { resource }.record_leaf(
                    &value,
                    attribute.value_type(),
                    path.segments(),
                    attribute.name(),
                    0,
                    &mut |value, ty, path, secret| {
                        collect_leaf(resource, value, ty, path, secret, values)
                    },
                )?;
            }
        }
    }
    Ok(())
}

struct ReferenceValidation<'a> {
    resource: &'a ResourceId,
}
impl ReferenceValidation<'_> {
    #[allow(clippy::too_many_arguments)]
    fn record_leaf(
        &self,
        value: &Value,
        ty: crate::schema::EffectiveType<'_>,
        segments: &[crate::schema::LeafPathSegment],
        path: &str,
        depth: usize,
        visit: &mut impl FnMut(&Value, &TypeIdentity, &str, bool) -> Result<(), Box<ReferencePlanError>>,
    ) -> Result<(), Box<ReferencePlanError>> {
        use crate::schema::LeafPathSegment;
        let Some((segment, rest)) = segments.split_first().filter(|_| depth < 64) else {
            return self.walk(value, ty, path, false, depth, visit);
        };
        if matches!(ty.shape(), Shape::Union) {
            for member in ty.union_members() {
                self.record_leaf(value, member, segments, path, depth + 1, visit)?;
            }
            return Ok(());
        }
        let (child, path) = match segment {
            LeafPathSegment::StructField(name) => (
                ty.field_candidates()
                    .into_iter()
                    .find(|field| field.name() == name)
                    .map(|field| field.value_type()),
                format!("{path}.{name}"),
            ),
            LeafPathSegment::MapKey(name) => (
                ty.map_value(),
                format!("{path}[{}]", crate::parser::display_string_literal(name)),
            ),
            LeafPathSegment::ListIndex(index) => (ty.list_element(), format!("{path}[{index}]")),
        };
        if let Some(child) = child {
            self.record_leaf(value, child, rest, &path, depth + 1, visit)?;
        }
        Ok(())
    }

    fn masked(
        &self,
        masked: &crate::resource::MaskedComparisonAttributes,
        schema: &crate::schema::ValidatedResourceSchema,
    ) -> Result<(), Box<ReferencePlanError>> {
        for attribute in schema.effective_attributes() {
            for value in [
                masked.desired().get(attribute.name()),
                masked.current().get(attribute.name()),
                masked.saved().and_then(|saved| saved.get(attribute.name())),
            ]
            .into_iter()
            .flatten()
            {
                self.walk(
                    value,
                    attribute.value_type(),
                    attribute.name(),
                    false,
                    0,
                    &mut |_, _, _, _| Ok(()),
                )?;
            }
        }
        Ok(())
    }
    fn validate(
        &self,
        value: &Value,
        ty: crate::schema::EffectiveType<'_>,
        path: &str,
        secret: bool,
        depth: usize,
    ) -> Result<(), Box<ReferencePlanError>> {
        self.walk(value, ty, path, secret, depth, &mut |value, _, path, secret| {
            if secret || crate::value::contains_secret(value)
                || matches!(value, Value::Concrete(ConcreteValue::String(raw)) if raw.is_diagnostic_sensitive())
            {
                return Err(Box::new(ReferencePlanError {
                    instance: ProviderInstanceKey::from(self.resource),
                    resource: self.resource.clone(),
                    path: path.into(),
                    raw: ValueString::diagnostic_sensitive("<redacted>"),
                    source: ReferenceError::InvalidReference(ProviderError::invalid_input(
                        "secret values cannot be used as references",
                    ).into()).into(),
                }));
            }
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        value: &Value,
        ty: crate::schema::EffectiveType<'_>,
        path: &str,
        secret: bool,
        depth: usize,
        visit: &mut impl FnMut(&Value, &TypeIdentity, &str, bool) -> Result<(), Box<ReferencePlanError>>,
    ) -> Result<(), Box<ReferencePlanError>> {
        if depth >= 64 {
            return Err(Box::new(ReferencePlanError {
                instance: ProviderInstanceKey::from(self.resource),
                resource: self.resource.clone(),
                path: path.into(),
                raw: ValueString::diagnostic_sensitive("<depth limit>"),
                source: ReferenceError::InvalidReference(
                    ProviderError::invalid_input(format!(
                        "reference traversal depth limit (64) exceeded at {path}"
                    ))
                    .into(),
                )
                .into(),
            }));
        }
        if let Value::Deferred(crate::resource::DeferredValue::Secret(inner)) = value {
            return self.walk(inner, ty, path, true, depth + 1, visit);
        }
        match ty.shape() {
            Shape::Reference { identity, .. } => visit(value, identity, path, secret)?,
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
                            let path = if is_map {
                                format!("{path}[{}]", crate::parser::display_string_literal(name))
                            } else {
                                format!("{path}.{name}")
                            };
                            self.walk(value, child_type, &path, secret, depth + 1, visit)?;
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
                            secret,
                            depth + 1,
                            visit,
                        )?;
                    }
                }
            }
            Shape::Union => {
                for member in ty.union_members() {
                    self.walk(value, member, path, secret, depth + 1, visit)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub(crate) fn validate_secret_operand(
    resource: &ResourceId,
    value: &Value,
    ty: crate::schema::EffectiveType<'_>,
    path: &str,
) -> Result<(), Box<ReferencePlanError>> {
    ReferenceValidation { resource }.validate(value, ty, path, false, 0)
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn prepare_empty_for_tests() -> ResolvedReferences {
    ResolvedReferences {
        values: HashMap::new(),
        dangling: HashMap::new(),
        recording: None,
    }
}

#[cfg(test)]
mod tests;
