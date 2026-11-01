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
    #[error("reference comparison did not converge after {limit} recording passes")]
    IterationLimit { limit: usize },
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
    side: ReferenceSide,
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
#[derive(Debug, Clone, Default)]
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
    pub fn dangling_paths(&self, resource: &ResourceId) -> impl Iterator<Item = &str> {
        let mut seen = HashSet::new();
        self.dangling_references(resource)
            .map(|reference| reference.path.as_str())
            .filter(move |path| seen.insert(*path))
    }

    pub(crate) fn dangling_references(
        &self,
        resource: &ResourceId,
    ) -> impl Iterator<Item = &DanglingReference> {
        self.dangling.get(resource).into_iter().flatten()
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
                    source: ReferenceError::InvalidReference(ProviderError::invalid_input(
                        "secret values cannot be used as references",
                    ))
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
                side: operand.side,
                request: ReferenceRequest {
                    ty: ty.clone(),
                    value: raw.expose_plaintext().into(),
                },
            };
            match self.values.get(&occurrence.key()) {
                Some(CachedOutcome::Resolved(value)) => canonical.push(Some(value)),
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
/// ```compile_fail,E0599
/// use carina_core::{references::ProvisionalComparison, plan::Plan};
/// fn escape(result: ProvisionalComparison<'_, Result<Plan, ()>>) -> Plan {
///     result.into_inner().unwrap()
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
}

const MAX_RECORDING_PASSES: usize = 64;

impl ReferencePreparation {
    pub async fn run<T, E>(
        &mut self,
        provider: &ProviderRouter,
        compute: impl for<'run> FnMut(
            RecordingSession<'run>,
        ) -> ProvisionalComparison<'run, Result<T, E>>,
    ) -> Result<PreparedComparison<T>, E>
    where
        E: From<Box<ReferencePlanError>>,
    {
        self.run_with_limit(provider, MAX_RECORDING_PASSES, compute)
            .await
    }

    async fn run_with_limit<T, E>(
        &mut self,
        provider: &ProviderRouter,
        limit: usize,
        mut compute: impl for<'run> FnMut(
            RecordingSession<'run>,
        ) -> ProvisionalComparison<'run, Result<T, E>>,
    ) -> Result<PreparedComparison<T>, E>
    where
        E: From<Box<ReferencePlanError>>,
    {
        for pass in 0..=limit {
            let mut references = ResolvedReferences {
                values: self.cache.clone(),
                recording: Some(Arc::default()),
                ..ResolvedReferences::default()
            };
            let provisional = compute(RecordingSession {
                references: &references,
            });
            let (missing, dangling) = {
                let recording = references
                    .recording
                    .as_ref()
                    .expect("recording pass")
                    .lock()
                    .expect("reference recording lock");
                (recording.missing.clone(), recording.dangling.clone())
            };
            if missing.is_empty() {
                // This is the sole release point. Errors from provisional
                // passes are discarded too: identities can change control flow.
                let value = provisional.value?;
                references.recording = None;
                references.dangling = dangling;
                return Ok(PreparedComparison { value, references });
            }
            drop(provisional);
            if pass == limit {
                return Err(missing[0]
                    .error(ReferenceFailure::IterationLimit { limit })
                    .into());
            }
            let previous_size = self.cache.len();
            self.resolve_occurrences(&missing, provider).await?;
            debug_assert!(
                self.cache.len() > previous_size,
                "a retry must strictly grow the witness"
            );
        }
        unreachable!("the final recording pass either releases or returns the typed cap error")
    }

    async fn resolve_occurrences(
        &mut self,
        occurrences: &[Occurrence],
        provider: &ProviderRouter,
    ) -> Result<(), Box<ReferencePlanError>> {
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
        // Validate every recorded side, including duplicate raw values first
        // encountered on a historical operand in another resource or pass.
        for origin in occurrences {
            if origin.side == ReferenceSide::Desired
                && let Some(CachedOutcome::NotFound(error)) = self.cache.get(&origin.key())
            {
                return Err(origin.error(ReferenceFailure::DesiredNotFound(Arc::clone(error))));
            }
        }
        Ok(())
    }

    /// Focused compatibility helper. Production supplies the complete planning
    /// operation to `run`, including its explicit fields and cascade inputs.
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
            .run(provider, |session| {
                session.plan(
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
            })
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
                    source: ReferenceFailure::Call(error),
                }))
            }
        }
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
    pub fn stability(
        self,
        desired: &[ResolvedResource],
        current: &HashMap<ResourceId, PlanInputCurrent>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        provider: &ProviderRouter,
    ) -> ProvisionalComparison<
        'run,
        crate::differ::PlanComparisonResult<
            HashMap<ResourceId, crate::binding_index::ManagedBindingStability>,
        >,
    > {
        self.result((|| {
            validate_inputs(desired, current, registry, saved)?;
            crate::differ::managed_binding_stability(
                desired,
                current,
                registry,
                saved,
                explicit,
                provider,
                self.references,
            )
        })())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn plan(
        self,
        desired: &[ResolvedResource],
        data_sources: &[crate::resource::DataSource],
        provider: &ProviderRouter,
        prior: crate::differ::PlanPriorStates<'_>,
        directives: &HashMap<ResourceId, crate::resource::Directives>,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        waits: &[crate::parser::WaitBinding<crate::resource::AbsoluteBindingAddress>],
    ) -> ProvisionalComparison<'run, crate::differ::PlanComparisonResult<crate::plan::Plan>> {
        self.result((|| {
            validate_inputs(
                desired,
                &prior.materialize_current_states(desired, registry),
                registry,
                saved,
            )?;
            let plan = crate::differ::create_plan(
                desired,
                data_sources,
                provider,
                prior,
                directives,
                registry,
                saved,
                explicit,
                waits,
                self.references,
            )?;
            self.render(&plan, registry, saved, explicit, provider)?;
            Ok(plan)
        })())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn plan_with_cascades(
        self,
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
    ) -> ProvisionalComparison<
        'run,
        crate::differ::PlanComparisonResult<crate::differ::PendingCascadePlan>,
    > {
        self.result((|| {
            let current = prior.materialize_current_states(desired.authored(), registry);
            validate_inputs(desired.authored(), &current, registry, saved)?;
            validate_inputs(dependents.managed(), &current, registry, saved)?;
            let plan = crate::differ::create_plan_with_cascades(
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
                self.references,
            )?;
            self.render(plan.comparison_plan(), registry, saved, explicit, provider)?;
            Ok(plan)
        })())
    }

    fn render(
        self,
        plan: &crate::plan::Plan,
        registry: &SchemaRegistry,
        saved: &LiftedSavedAttrs,
        explicit: &HashMap<ResourceId, crate::explicit::ExplicitFields>,
        provider: &ProviderRouter,
    ) -> crate::provider::ProviderResult<()> {
        use crate::detail_rows::{
            DetailComparisons, DetailLevel, HistoricalDisplayAttributes,
            build_detail_rows_by_instance, build_replace_detail_rows_from_display,
        };
        let historical = HistoricalDisplayAttributes::for_planning(plan, saved);
        for detail in [DetailLevel::Full, DetailLevel::Explicit] {
            let replacements = plan
                .replace_display_info()
                .map(|info| (info.create_idx, info))
                .collect::<HashMap<_, _>>();
            for (index, effect) in plan.effects().iter().enumerate() {
                if let (crate::effect::Effect::Create(to), Some(replacement)) =
                    (effect, replacements.get(&index))
                {
                    build_replace_detail_rows_from_display(
                        plan,
                        to,
                        *replacement,
                        &historical,
                        Some(registry),
                        detail,
                        explicit.get(&to.id),
                        provider,
                        DetailComparisons::Planning(self.references),
                    )?;
                } else {
                    build_detail_rows_by_instance(
                        effect,
                        Some(registry),
                        detail,
                        &historical,
                        Some(explicit),
                        provider,
                        DetailComparisons::Planning(self.references),
                    )?;
                }
            }
        }
        Ok(())
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
                        source: ReferenceFailure::Call(error),
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

/// Validation walks each operand independently. It never enumerates comparison
/// pairs or requests identities. Complete this preflight before any guest call,
/// including creates and values a short-circuiting comparator will not visit.
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

struct ReferenceValidation<'a> {
    resource: &'a ResourceId,
}
impl ReferenceValidation<'_> {
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
                self.validate(value, attribute.value_type(), attribute.name(), false, 0)?;
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
        if depth >= 64 {
            return Err(Box::new(ReferencePlanError {
                instance: ProviderInstanceKey::from(self.resource),
                resource: self.resource.clone(),
                path: path.into(),
                raw: ValueString::diagnostic_sensitive("<depth limit>"),
                source: ReferenceError::InvalidReference(ProviderError::invalid_input(format!(
                    "reference traversal depth limit (64) exceeded at {path}"
                )))
                .into(),
            }));
        }
        if let Value::Deferred(crate::resource::DeferredValue::Secret(inner)) = value {
            return self.validate(inner, ty, path, true, depth + 1);
        }
        match ty.shape() {
            Shape::Reference { .. }
                if secret
                    || crate::value::contains_secret(value)
                    || matches!(value, Value::Concrete(ConcreteValue::String(raw)) if raw.is_diagnostic_sensitive()) =>
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
                            self.validate(value, child_type, &path, secret, depth + 1)?;
                        }
                    }
                }
            }
            Shape::List { .. } => {
                if let Some(items) = crate::resource::comparison_list_items(value)
                    && let Some(element) = ty.list_element()
                {
                    for (index, value) in items.iter().enumerate() {
                        self.validate(
                            value,
                            element.clone(),
                            &format!("{path}[{index}]"),
                            secret,
                            depth + 1,
                        )?;
                    }
                }
            }
            Shape::Union => {
                for member in ty.union_members() {
                    self.validate(value, member, path, secret, depth + 1)?;
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
    ResolvedReferences::default()
}

#[cfg(test)]
mod tests;
