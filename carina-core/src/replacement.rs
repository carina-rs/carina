//! Replacement intent and the sole admission boundary for replacement order.
//!
//! Persisted plans and deferred templates carry requests, never admission proofs.
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::effect::{
    DeferredReplaceDelete, DeletedInstanceKey, Effect, NonEmptyDeletes, TemporaryName,
};
use crate::plan::{
    CreateBeforeDestroyError, ReplacementCannotCoexistError, SchemaNotRegisteredError,
};
use crate::resource::{
    ConcreteValue, PlanValueKind, ResolvedResource, ResolvedResourceId, SnapshotValue, Value,
};
use crate::schema::{SchemaKind, SchemaRegistry, UniqueNameSpec};

#[cfg(test)]
#[path = "replacement/plan_scheduling_tests.rs"]
mod plan_scheduling_tests;
#[path = "replacement/scheduling.rs"]
mod scheduling;
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub use scheduling::create_permit_for_test;
pub use scheduling::{CreatePermit, ScheduledEffect, ScheduledEffects, ScheduledPlan};
pub(crate) use scheduling::{ScheduledBasicEffect, ScheduledResume};

/// State rows own all their provider generations. Routing aliases belong to
/// the generations, not to the row's identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StateRowLineage(crate::resource::ResourceId);

impl StateRowLineage {
    fn of(id: &ResolvedResourceId) -> Self {
        let mut row = id.as_inner().clone();
        row.provider_instance = None;
        Self(row)
    }
}

pub(crate) fn same_state_row(left: &ResolvedResourceId, right: &ResolvedResourceId) -> bool {
    StateRowLineage::of(left) == StateRowLineage::of(right)
}

fn row_on_route(
    id: &ResolvedResourceId,
    route: &crate::resource::ResourceId,
) -> ResolvedResourceId {
    let mut row = id.as_inner().clone();
    row.provider_instance = route.provider_instance.clone();
    ResolvedResourceId::new(row)
}

#[derive(Default)]
pub(crate) struct ReplacementLineage {
    sources: HashMap<StateRowLineage, HashSet<StateRowLineage>>,
}

impl ReplacementLineage {
    pub(crate) fn required_by<'a>(
        &self,
        target: &ResolvedResourceId,
        requirements: &'a HashMap<ResolvedResourceId, String>,
    ) -> Option<&'a str> {
        requirements
            .get(target)
            .or_else(|| {
                requirements
                    .iter()
                    .find(|(required, _)| self.owns(target, required))
                    .map(|(_, dependent)| dependent)
            })
            .map(String::as_str)
    }

    fn associate(&mut self, target: &ResolvedResourceId, old: &ResolvedResourceId) {
        self.sources
            .entry(StateRowLineage::of(target))
            .or_default()
            .insert(StateRowLineage::of(old));
    }

    fn owns(&self, target: &ResolvedResourceId, old: &ResolvedResourceId) -> bool {
        let old = StateRowLineage::of(old);
        let mut pending = vec![StateRowLineage::of(target)];
        let mut seen = HashSet::new();
        while let Some(row) = pending.pop() {
            if row == old {
                return true;
            }
            if seen.insert(row.clone()) {
                pending.extend(self.sources.get(&row).into_iter().flatten().cloned());
            }
        }
        false
    }
}

/// Unadmitted intent. Changing a request never grants create-first permission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplacementRequest<T> {
    /// Desired target, still an unchosen template for deferred replacements.
    pub template: T,
    pub deletes: NonEmptyDeletes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    required_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared_name: Option<TemporaryName>,
    #[serde(skip)]
    naming: ReplacementNaming,
}

/// A continuation retains its original request name. Admission still checks
/// permission to resend, but must not prepare another name for that operation.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum ReplacementNaming {
    #[default]
    Prepare,
    Continuation,
}
impl<T> ReplacementRequest<T> {
    pub fn new(target: T, old_instances: NonEmptyDeletes) -> Self {
        Self {
            template: target,
            deletes: old_instances,
            required_by: None,
            prepared_name: None,
            naming: ReplacementNaming::Prepare,
        }
    }
    pub fn target(&self) -> &T {
        &self.template
    }
    pub fn old_instances(&self) -> &NonEmptyDeletes {
        &self.deletes
    }
    pub fn required_by(&self) -> Option<&str> {
        self.required_by.as_deref()
    }
    pub(crate) fn require_create_first_in_place(&mut self, dependent: String) {
        self.required_by = Some(dependent);
    }
    pub fn require_create_first(mut self, dependent: impl Into<String>) -> Self {
        self.required_by = Some(dependent.into());
        self
    }
    pub fn map_target<U>(self, map: impl FnOnce(T) -> U) -> ReplacementRequest<U> {
        ReplacementRequest {
            template: map(self.template),
            deletes: self.deletes,
            required_by: self.required_by,
            prepared_name: self.prepared_name,
            naming: self.naming,
        }
    }
    pub(crate) fn with_prepared_name(mut self, name: Option<TemporaryName>) -> Self {
        self.prepared_name = name;
        self
    }
}

// The witness owns its target. There is no separately transferable order token.
#[derive(Debug, Clone, PartialEq)]
struct CreateFirst<T> {
    target: T,
    temporary_name: Option<TemporaryName>,
}
#[derive(Debug, Clone, PartialEq)]
enum Admitted<T> {
    DeleteFirst(T),
    CreateFirst(CreateFirst<T>),
}

/// An admitted replacement owns the exact target and old instances it checked.
/// Only [`resolve_replacement`] can construct one; it cannot be deserialized.
///
/// ```compile_fail,E0603
/// use carina_core::replacement::CreateFirst;
/// let witness = CreateFirst { target: (), temporary_name: None };
/// ```
///
/// A caller cannot exchange a checked target for a different resource:
///
/// ```compile_fail,E0616
/// use carina_core::replacement::CheckedReplacement;
/// fn swap(checked: &mut CheckedReplacement<()>) {
///     checked.admitted = ();
/// }
/// ```
///
/// Raw endpoints do not create a checked owner:
///
/// ```compile_fail,E0308
/// use carina_core::{effect::Effect, replacement::CheckedReplacement, resource::ResolvedResource};
/// fn bypass(create: Effect, delete: Effect) -> CheckedReplacement<ResolvedResource> {
///     (create, delete)
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedReplacement<T> {
    admitted: Admitted<T>,
    old_instances: NonEmptyDeletes,
    required_by: Option<String>,
    naming: ReplacementNaming,
}
impl<T> CheckedReplacement<T> {
    pub fn target(&self) -> &T {
        match &self.admitted {
            Admitted::DeleteFirst(target) => target,
            Admitted::CreateFirst(witness) => &witness.target,
        }
    }
    pub fn old_instances(&self) -> &NonEmptyDeletes {
        &self.old_instances
    }
    pub fn is_create_first(&self) -> bool {
        matches!(self.admitted, Admitted::CreateFirst(_))
    }
    pub fn temporary_name(&self) -> Option<&TemporaryName> {
        match &self.admitted {
            Admitted::DeleteFirst(_) => None,
            Admitted::CreateFirst(witness) => witness.temporary_name.as_ref(),
        }
    }
    pub fn into_request(self) -> ReplacementRequest<T> {
        let (target, prepared_name) = match self.admitted {
            Admitted::DeleteFirst(target) => (target, None),
            Admitted::CreateFirst(witness) => (witness.target, witness.temporary_name),
        };
        ReplacementRequest {
            template: target,
            deletes: self.old_instances,
            required_by: self.required_by,
            prepared_name,
            naming: self.naming,
        }
    }
    pub(crate) fn project_prior_explicit(
        &mut self,
        prior: &HashMap<crate::resource::ResourceId, crate::resource::PersistedDirectives>,
    ) {
        for old in self.old_instances.iter_mut() {
            let dependencies = prior
                .get(old.id.as_inner())
                .map(|directives| directives.depends_on.clone())
                .unwrap_or_default();
            old.dependencies.extend_projected(&dependencies);
            old.explicit_dependencies = dependencies.to_resolved_dependencies();
        }
    }
}

impl<S, P: PlanValueKind> CheckedReplacement<ResolvedResource<S, P>> {
    pub(crate) fn into_scheduling(self) -> ReplacementOrder {
        ReplacementOrder {
            target: self.target().resolved_id(),
            old_instances: self
                .old_instances
                .iter()
                .map(DeferredReplaceDelete::instance_key)
                .collect(),
            kind: match self.admitted {
                Admitted::DeleteFirst(_) => ReplacementOrderKind::DeleteFirst,
                Admitted::CreateFirst(_) => ReplacementOrderKind::CreateFirst,
            },
            continuation: (self.naming == ReplacementNaming::Continuation)
                .then_some(ContinuationOrder::Resend),
        }
    }
}

impl<S: Clone> CheckedReplacement<ResolvedResource<S>> {
    pub(crate) fn into_effects(
        self,
        consumer_updates: crate::resource::ResolvedDependencies,
    ) -> Vec<Effect<S>> {
        let create_first = self.is_create_first();
        let request = self.into_request();
        let mut deletes: Vec<_> = request
            .deletes
            .iter()
            .map(|old| {
                let mut old = old.clone();
                old.directives.create_before_destroy = create_first;
                old.blocked_by_updates
                    .extend(consumer_updates.iter().cloned());
                old.to_delete_effect()
            })
            .collect();
        let create = Effect::Create(request.template);
        if create_first {
            deletes.insert(0, create);
        } else {
            deletes.push(create);
        }
        deletes
    }
}

/// Admit the target's own replacement request. This is the only order selector.
/// A Coexisting schema permits create-first but does not request it.
pub fn resolve_replacement<S: Clone, P: PlanValueKind>(
    request: ReplacementRequest<ResolvedResource<S, P>>,
    registry: &SchemaRegistry,
) -> Result<CheckedReplacement<ResolvedResource<S, P>>, CreateBeforeDestroyError> {
    let ReplacementRequest {
        template: target,
        deletes: old_instances,
        required_by,
        prepared_name,
        naming,
    } = request;
    let admitted = if target.directives.create_before_destroy || required_by.is_some() {
        let schema = registry
            .get(
                &target.id.provider,
                &target.id.resource_type,
                SchemaKind::Resource,
            )
            .ok_or_else(|| SchemaNotRegisteredError {
                resource_type: target.id.display_type(),
                resource_identity: target
                    .binding
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| target.id.identity_display().to_string()),
            })?;
        let temporary_name = prepare_name(
            &target,
            &old_instances,
            schema.as_schema(),
            prepared_name.as_ref(),
            naming,
        )?;
        // The target supplies authored directives to writeback. Promotion
        // belongs to this checked owner's admitted order.
        let mut resource = target.as_resource().clone();
        if let Some(name) = &temporary_name {
            resource.set_attr(
                name.attribute.clone(),
                Value::Concrete(ConcreteValue::String(name.temporary_value.clone().into())),
            );
        }
        Admitted::CreateFirst(CreateFirst {
            target: target.replace_inner(resource),
            temporary_name,
        })
    } else {
        Admitted::DeleteFirst(target)
    };
    Ok(CheckedReplacement {
        admitted,
        old_instances,
        required_by,
        naming,
    })
}

fn prepare_name<S, P: PlanValueKind>(
    target: &ResolvedResource<S, P>,
    old: &NonEmptyDeletes,
    schema: &crate::schema::CompiledResourceSchema,
    prepared: Option<&TemporaryName>,
    naming: ReplacementNaming,
) -> Result<Option<TemporaryName>, CreateBeforeDestroyError> {
    let attribute = match &schema.unique_name {
        UniqueNameSpec::Coexisting => return Ok(None),
        UniqueNameSpec::Conflicting => {
            return Err(ReplacementCannotCoexistError {
                resource_type: target.id.display_type(),
                resource_identity: target
                    .binding
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| target.id.identity_display().to_string()),
            }
            .into());
        }
        UniqueNameSpec::Attribute(attribute) => attribute,
    };
    if naming == ReplacementNaming::Continuation {
        return Ok(None);
    }
    if let Some(prepared) = prepared {
        if prepared.attribute != *attribute
            || target.prefixes.contains_key(attribute)
            || prepared.original_value == prepared.temporary_value
            || old.iter().any(|old| matches!(old.previous_attributes.get(attribute), Some(Value::Concrete(ConcreteValue::String(previous))) if previous.expose_plaintext() == prepared.temporary_value))
            || !matches!(target.get_attr(attribute), Some(Value::Concrete(ConcreteValue::String(value))) if value.expose_plaintext() == prepared.temporary_value)
        {
            return Err(crate::plan::InvalidPreparedReplacementNameError {
                resource_type: target.id.display_type(),
                attribute: attribute.clone(),
            }.into());
        }
        return Ok(Some(TemporaryName {
            can_rename: schema
                .attributes
                .get(attribute)
                .is_some_and(|attr| !attr.create_only),
            ..prepared.clone()
        }));
    }
    if target.prefixes.contains_key(attribute) {
        return Ok(None);
    }
    let Some(Value::Concrete(ConcreteValue::String(original))) = target.get_attr(attribute) else {
        return Ok(None);
    };
    if old.iter().all(|instance| matches!(instance.previous_attributes.get(attribute), Some(Value::Concrete(ConcreteValue::String(previous))) if previous.expose_plaintext() != original.expose_plaintext())) { return Ok(None); }
    Ok(Some(TemporaryName {
        attribute: attribute.clone(),
        original_value: original.clone().into_plaintext(),
        temporary_value: format!(
            "{}-{}",
            original.expose_plaintext(),
            crate::identifier::generate_random_suffix()
        ),
        can_rename: schema
            .attributes
            .get(attribute)
            .is_some_and(|attribute| !attribute.create_only),
    }))
}

/// New consumers require coexistence independently of any old-generation cycle.
pub(crate) fn create_first_requirements<S, P: PlanValueKind>(
    effects: &[Effect<S, P>],
) -> HashMap<ResolvedResourceId, String> {
    let mut requirements = HashMap::new();
    for (effect, dependencies) in effects.iter().zip(new_dependencies(effects)) {
        for dependency in dependencies {
            if dependency.as_inner() != effect.resource_id() {
                requirements
                    .entry(dependency)
                    .or_insert_with(|| effect.resource_id().to_string());
            }
        }
    }
    requirements
}

fn new_dependencies<S, P: PlanValueKind>(
    effects: &[Effect<S, P>],
) -> Vec<crate::resource::ResolvedDependencies> {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.all_dependencies().cloned().collect()
            }
            Effect::Update { to, .. } => to.all_dependencies().cloned().collect(),
            Effect::DeferredCreate(payload) => payload.dependency_template.dependencies(),
            Effect::DeferredReplace(payload) => payload.dependency_template().dependencies(),
            Effect::Read { .. }
            | Effect::Delete { .. }
            | Effect::Wait { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Default::default(),
        })
        .collect()
}

/// Candidate order changes for the current graph. A cycle can have several
/// delete-first pairs; admission must try one permissible change and rebuild
/// this graph before deciding that any rejected candidate is mandatory.
fn cycle_promotions<S: Clone, P: PlanValueKind>(
    admitted: &AdmittedReplacementEffects<S, P>,
) -> Vec<(ResolvedResourceId, String)> {
    // Admission and dispatch consume this same graph, including deferred gates,
    // Reads, persisted dependencies, and the checked replacement windows.
    let analysis = match admitted.dependency_analysis(std::iter::empty()) {
        Ok(analysis) => analysis,
        Err(failure) => failure.into_parts().0,
    };
    let edges = analysis.into_deps_of();
    let mut promotions = Vec::new();
    for replacement in &admitted.scheduling.replacements {
        let create = replacement.create;
        for &delete in &replacement.deletes {
            let target = &replacement.order.target;
            if replacement.order.runs_before_cleanup()
                || !dependency_reaches(&edges, delete, create)
            {
                continue;
            }
            let dependent = edges
                .get(&delete)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&candidate| {
                    admitted.effects[candidate].resource_id() != target.as_inner()
                        && dependency_reaches(&edges, candidate, create)
                })
                .min()
                .map(|index| admitted.effects[index].resource_id().to_string())
                .unwrap_or_else(|| target.to_string());
            promotions.push((target.clone(), dependent));
        }
    }
    promotions
}

fn dependency_reaches(edges: &HashMap<usize, HashSet<usize>>, start: usize, target: usize) -> bool {
    let mut pending = vec![start];
    let mut visited = HashSet::new();
    while let Some(node) = pending.pop() {
        if node == target {
            return true;
        }
        if visited.insert(node) {
            pending.extend(edges.get(&node).into_iter().flatten().copied());
        }
    }
    false
}

/// An admission failure retains the rejected target and dependency request.
#[derive(Debug, thiserror::Error)]
#[error("replacement admission failed for `{resource_id}`")] // error-chain: caller-renders-sources
pub struct ReplacementAdmissionError {
    pub(crate) resource_id: Box<crate::resource::ResourceId>,
    pub(crate) required_by: Option<String>,
    #[source]
    pub(crate) cause: CreateBeforeDestroyError,
}
impl ReplacementAdmissionError {
    pub(crate) fn into_plan_error(self) -> crate::plan::PlanError {
        let kind = match self.required_by {
            Some(dependent) => crate::plan::PlanErrorKind::CreateBeforeDestroyRequiredByDependent {
                dependent,
                cause: self.cause,
            },
            None => self.cause.into(),
        };
        crate::plan::PlanError::new(*self.resource_id, kind)
    }
}

/// Replacement endpoints and their scheduling proof are compiled together.
/// Execution cannot retain a raw Create while using its prepared target's proof.
///
/// ```compile_fail,E0451
/// use carina_core::replacement::AdmittedReplacementEffects;
/// let unchecked = AdmittedReplacementEffects { effects: Vec::new() };
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::replacement::AdmittedReplacementEffects;
/// fn detach(owner: AdmittedReplacementEffects) {
///     let (effects, proof, _) = owner.into_parts();
/// }
/// ```
///
/// ```compile_fail,E0616
/// use carina_core::{effect::Effect, replacement::AdmittedReplacementEffects};
/// fn swap(owner: &mut AdmittedReplacementEffects, raw: Effect) {
///     owner.effects[0] = raw;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AdmittedReplacementEffects<
    S = crate::resource::ModuleSource,
    P: PlanValueKind = crate::resource::NoPlaceholder,
> {
    effects: Vec<Effect<S, P>>,
    scheduling: ReplacementScheduling,
    temporary_names: PreparedReplacementNames,
    writeback: Vec<ReplacementWriteback>,
}
pub(crate) type PreparedReplacementNames = Vec<(crate::resource::ResourceId, TemporaryName)>;

/// Unadmitted naming/history inputs survive independently of display rows.
/// Historical values are keyed by the entire provider generation, never by
/// the desired target or by the current row's address alone.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplacementInputs {
    pub(crate) old_instances: Vec<OldInstanceSnapshot>,
    pub(crate) prepared_names: PreparedReplacementNames,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct OldInstanceSnapshot {
    pub(crate) instance: DeletedInstanceKey,
    pub(crate) attributes: HashMap<String, SnapshotValue>,
}

impl ReplacementInputs {
    pub(crate) fn is_empty(&self) -> bool {
        self.old_instances.is_empty() && self.prepared_names.is_empty()
    }

    pub(crate) fn record_old(&mut self, old: &DeferredReplaceDelete) {
        let instance = old.instance_key();
        self.old_instances
            .retain(|previous| previous.instance != instance);
        self.old_instances.push(OldInstanceSnapshot {
            instance,
            attributes: old.previous_attributes.clone(),
        });
    }

    pub(crate) fn previous(
        &self,
        instance: &DeletedInstanceKey,
    ) -> Option<HashMap<String, SnapshotValue>> {
        self.old_instances
            .iter()
            .find(|old| &old.instance == instance)
            .map(|old| old.attributes.clone())
    }

    pub(crate) fn contains_redacted_secret_placeholders(&self) -> bool {
        self.old_instances.iter().any(|old| {
            old.attributes
                .values()
                .any(crate::value::contains_redacted_secret)
        })
    }
}

impl<S: Clone, P: PlanValueKind> AdmittedReplacementEffects<S, P> {
    pub fn resolve(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&DeletedInstanceKey) -> HashMap<String, SnapshotValue>,
    ) -> Result<Self, ReplacementAdmissionError> {
        Self::resolve_with_prepared_names(
            effects,
            registry,
            previous,
            &HashMap::new(),
            &HashMap::new(),
        )
    }
    pub(crate) fn resolve_with_prepared_names(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&DeletedInstanceKey) -> HashMap<String, SnapshotValue>,
        associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
        prepared_names: &HashMap<ResolvedResourceId, TemporaryName>,
    ) -> Result<Self, ReplacementAdmissionError> {
        Self::resolve_with_context(
            effects,
            registry,
            previous,
            associations,
            prepared_names,
            &HashMap::new(),
        )
    }
    fn resolve_with_context(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&DeletedInstanceKey) -> HashMap<String, SnapshotValue>,
        associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
        prepared_names: &HashMap<ResolvedResourceId, TemporaryName>,
        required: &HashMap<ResolvedResourceId, String>,
    ) -> Result<Self, ReplacementAdmissionError> {
        let mut effects = effects.to_vec();
        let mut lineage = ReplacementLineage::default();
        for (target, sources) in associations {
            for source in sources {
                lineage.associate(target, source);
            }
        }
        // A move transfers ownership of all source generations, but each
        // provider delete must retain the route that issued that generation.
        for effect in &effects {
            if let Effect::Move { from, to, .. } = effect {
                lineage.associate(to, from);
            }
        }
        let mut requirements = create_first_requirements(&effects);
        requirements.extend(required.clone());
        let admit = |request: ReplacementRequest<ResolvedResource<S, P>>| {
            let resource_id = Box::new(request.target().id.clone());
            let required_by = request.required_by().map(str::to_owned);
            resolve_replacement(request, registry).map_err(|cause| ReplacementAdmissionError {
                resource_id,
                required_by,
                cause,
            })
        };
        let mut checked_replacements = Vec::new();
        let mut scheduling = ReplacementScheduling::default();
        let mut temporary_names = Vec::new();
        let mut writeback = Vec::new();
        for index in 0..effects.len() {
            let target = match &effects[index] {
                Effect::Create(target)
                | Effect::ResumeCreate {
                    resource: target, ..
                } => target,
                _ => continue,
            };
            let old: Vec<_> = effects
                .iter()
                .filter(|effect| {
                    matches!(effect,
                        Effect::Delete { id, .. } if lineage.owns(&target.resolved_id(), id)
                    )
                })
                .filter_map(|effect| effect.to_delete_payload(&previous))
                .collect();
            if let Ok(old) = NonEmptyDeletes::try_new(old) {
                let mut request = ReplacementRequest::new(target.clone(), old)
                    .with_prepared_name(prepared_names.get(&target.resolved_id()).cloned());
                if matches!(&effects[index], Effect::ResumeCreate { .. }) {
                    request.naming = ReplacementNaming::Continuation;
                }
                if let Some(dependent) = lineage.required_by(&target.resolved_id(), &requirements) {
                    request = request.require_create_first(dependent);
                }
                checked_replacements.push((Some(index), admit(request)?));
            }
        }

        // Project known deferred rows into the same cycle graph as concrete
        // replacements. These value-free endpoints are only preflight inputs;
        // materialization still admits the actual children before execution.
        let mut known = Self {
            effects: effects.clone(),
            scheduling: ReplacementScheduling::default(),
            temporary_names: Vec::new(),
            writeback: Vec::new(),
        };
        for (index, checked) in &checked_replacements {
            known
                .scheduling
                .insert(checked.clone().into_scheduling(), &effects, index.unwrap());
        }
        // Expand absorbed teardowns before projecting creates, so their gate
        // barriers are present exactly once in the admission graph as well.
        let (mut known, _, _) = known.expand_deferred_replacements();
        // Projecting a known child must not detach it from the gate that
        // admits it at runtime. Capture those edges before adding previews.
        for gate in 0..known.effects.len() {
            for delete in known.deferred_delete_candidates(gate) {
                known
                    .scheduling
                    .gate_barriers
                    .entry(delete)
                    .or_default()
                    .insert(gate);
            }
        }
        for gate in 0..known.effects.len() {
            let Some(mut preview) = known.deferred_preflight_preview(gate, &previous) else {
                continue;
            };
            preview.require_create_first_with_lineage(&requirements, &lineage);
            for request in preview.preflight_requests() {
                if checked_replacements.iter().any(|(_, checked)| {
                    checked.target().resolved_id() == request.target().resolved_id()
                }) {
                    continue;
                }
                for old in request.deletes.iter() {
                    let delete = old.to_delete_effect();
                    if !known.effects.iter().any(|effect| {
                        matches!(effect, Effect::Delete { .. })
                            && effect.instance_key() == delete.instance_key()
                    }) {
                        known.effects.push(delete);
                    }
                }
                let checked = admit(request)?;
                let index = known.effects.len();
                known.effects.push(Effect::Create(checked.target().clone()));
                known
                    .scheduling
                    .insert(checked.clone().into_scheduling(), &known.effects, index);
                checked_replacements.push((None, checked));
            }
        }
        // Each successful iteration permanently promotes one distinct pair.
        // Rejected candidates never change the graph or consume a retry.
        for _ in 0..checked_replacements.len() {
            let promotions = cycle_promotions(&known);
            if promotions.is_empty() {
                break;
            }
            let mut rejected = None;
            let mut promoted = false;
            for (target, dependent) in promotions {
                let (_, checked) = checked_replacements
                    .iter_mut()
                    .find(|(_, checked)| checked.target().resolved_id() == target)
                    .expect("cycle candidates have a replacement request");
                match admit(
                    checked
                        .clone()
                        .into_request()
                        .require_create_first(&dependent),
                ) {
                    Ok(replacement) => {
                        let bound = known
                            .scheduling
                            .replacements
                            .iter_mut()
                            .find(|bound| bound.order.target == target)
                            .expect("cycle candidate is in the shared admission graph");
                        bound.order = replacement.clone().into_scheduling();
                        known.effects[bound.create] = match &known.effects[bound.create] {
                            Effect::ResumeCreate { started_at, .. } => Effect::ResumeCreate {
                                resource: replacement.target().clone(),
                                started_at: started_at.clone(),
                            },
                            _ => Effect::Create(replacement.target().clone()),
                        };
                        *checked = replacement;
                        requirements.insert(target, dependent);
                        promoted = true;
                        break;
                    }
                    Err(error) => {
                        rejected.get_or_insert(error);
                    }
                }
            }
            if !promoted {
                return Err(rejected.expect("every remaining promotion was rejected"));
            }
        }
        for (index, checked) in checked_replacements {
            let Some(index) = index else {
                continue;
            };
            if let Some(name) = checked.temporary_name() {
                temporary_names.push((checked.target().id.clone(), name.clone()));
            }
            writeback.push(ReplacementWriteback::from_checked(&checked));
            match &mut effects[index] {
                Effect::Create(target)
                | Effect::ResumeCreate {
                    resource: target, ..
                } => *target = checked.target().clone(),
                _ => unreachable!(),
            }
            scheduling.insert(checked.into_scheduling(), &effects, index);
        }
        let mut admitted = Self {
            effects,
            scheduling,
            temporary_names,
            writeback,
        };
        for gate in 0..admitted.effects.len() {
            let Some(mut preview) = admitted.deferred_preflight_preview(gate, &previous) else {
                continue;
            };
            preview.require_create_first_with_lineage(&requirements, &lineage);
            preview
                .preflight(registry)
                .map_err(|cause| ReplacementAdmissionError {
                    resource_id: Box::new(preview.id().as_inner().clone()),
                    required_by: preview.required_by().map(str::to_owned).or_else(|| {
                        preview
                            .required_children()
                            .first()
                            .map(|(_, dependent)| dependent.clone())
                    }),
                    cause,
                })?;
            if let Effect::DeferredReplace(payload) = &mut admitted.effects[gate] {
                **payload = preview;
            }
        }
        Ok(admitted)
    }
    pub fn effects(&self) -> &[Effect<S, P>] {
        &self.effects
    }
    pub(super) fn from_compiled_parts(
        effects: Vec<Effect<S, P>>,
        scheduling: ReplacementScheduling,
        temporary_names: PreparedReplacementNames,
        writeback: Vec<ReplacementWriteback>,
    ) -> Self {
        Self {
            effects,
            scheduling,
            temporary_names,
            writeback,
        }
    }
    pub(crate) fn temporary_names(&self) -> &PreparedReplacementNames {
        &self.temporary_names
    }
    pub(crate) fn with_materialized(
        &self,
        children: Vec<Effect<S, P>>,
        requests: Vec<ReplacementRequest<ResolvedResource<S, P>>>,
        registry: &SchemaRegistry,
    ) -> Result<Self, ReplacementAdmissionError> {
        let mut effects = self.effects.clone();
        effects.extend(children);
        let mut associations: HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>> =
            HashMap::new();
        let mut previous = HashMap::new();
        let mut required = HashMap::new();
        for replacement in &self.writeback {
            associations
                .entry(replacement.target.clone())
                .or_default()
                .extend(replacement.deletes.iter().map(|old| old.id.clone()));
            previous.extend(
                replacement
                    .deletes
                    .iter()
                    .map(|old| (old.instance_key(), old.previous_attributes.clone())),
            );
        }
        for request in requests {
            let target = request.target().resolved_id();
            associations
                .entry(target.clone())
                .or_default()
                .extend(request.deletes.iter().map(|old| old.id.clone()));
            previous.extend(
                request
                    .deletes
                    .iter()
                    .map(|old| (old.instance_key(), old.previous_attributes.clone())),
            );
            if let Some(dependent) = request.required_by() {
                required.insert(target, dependent.to_owned());
            }
        }
        let names = self
            .temporary_names
            .iter()
            .map(|(id, name)| (ResolvedResourceId::new(id.clone()), name.clone()))
            .collect();
        Self::resolve_with_context(
            &effects,
            registry,
            |id| previous.get(id).cloned().unwrap_or_default(),
            &associations,
            &names,
            &required,
        )
    }
    pub(crate) fn writeback(&self) -> &[ReplacementWriteback] {
        &self.writeback
    }
    pub(crate) fn replaces_current(&self, id: &crate::resource::ResourceId) -> bool {
        self.scheduling.replacements.iter().any(|replacement| {
            replacement
                .order
                .old_instances
                .iter()
                .any(|old| old.id() == id && old.generation().is_current())
        })
    }
    pub(super) fn into_parts(
        self,
    ) -> (
        Vec<Effect<S, P>>,
        ReplacementScheduling,
        PreparedReplacementNames,
    ) {
        (self.effects, self.scheduling, self.temporary_names)
    }
}

impl<S: Clone, P: PlanValueKind> AdmittedReplacementEffects<S, P> {
    fn deferred_preflight_preview(
        &self,
        gate: usize,
        previous: &impl Fn(&DeletedInstanceKey) -> HashMap<String, SnapshotValue>,
    ) -> Option<crate::effect::DeferredReplacePayload<S, P>> {
        match &self.effects[gate] {
            Effect::DeferredCreate(payload) => {
                let deletes = self
                    .deferred_delete_candidates(gate)
                    .into_iter()
                    // Anonymous candidates remain protected by the runtime
                    // gate, but their identity is unknown until expansion.
                    // Only confirmed members may reject the plan here.
                    .filter(|&index| match &self.effects[index] {
                        Effect::Delete { id, binding, .. } => {
                            payload.template.contains_managed_member(&row_on_route(
                                id,
                                payload.template.template_resource_id(),
                            )) || binding.as_ref().is_some_and(|binding| {
                                crate::differ::binding_matches_deferred_template(
                                    binding,
                                    payload.template.target(),
                                )
                            })
                        }
                        _ => false,
                    })
                    .filter_map(|index| self.effects[index].to_delete_payload(previous))
                    .collect();
                Some(crate::effect::DeferredReplacePayload::new(
                    NonEmptyDeletes::try_new(deletes).ok()?,
                    payload.template.clone(),
                    payload.dependency_template.clone(),
                ))
            }
            Effect::DeferredReplace(payload) => Some((**payload).clone()),
            _ => None,
        }
    }

    /// The same candidate set owns both the gate barrier and materialization's
    /// historical inputs. Deletes already owned by a concrete replacement
    /// cannot also belong to an unrelated deferred loop.
    pub(crate) fn deferred_delete_candidates(&self, gate: usize) -> Vec<usize> {
        let (template, absorbed) = match &self.effects[gate] {
            Effect::DeferredCreate(payload) => (payload.template.as_ref(), &[][..]),
            Effect::DeferredReplace(payload) => (payload.template.as_ref(), &payload.deletes[..]),
            _ => return Vec::new(),
        };
        self.effects
            .iter()
            .enumerate()
            .filter_map(|(index, effect)| {
                let Effect::Delete {
                    id,
                    identifier,
                    generation,
                    binding,
                    ..
                } = effect
                else {
                    return None;
                };
                let instance = DeletedInstanceKey::new(
                    id.as_inner().clone(),
                    generation.clone(),
                    identifier.as_str(),
                );
                if absorbed.iter().any(|old| old.instance_key() == instance) {
                    return Some(index);
                }
                if self
                    .scheduling
                    .replacements
                    .iter()
                    .any(|replacement| replacement.deletes.contains(&index))
                {
                    return None;
                }
                let kind = template.template_resource_id();
                if kind.provider != id.provider || kind.resource_type != id.resource_type {
                    return None;
                }
                let candidate = template
                    .contains_managed_member(&row_on_route(id, template.template_resource_id()))
                    || match binding {
                        Some(binding) => crate::differ::binding_matches_deferred_template(
                            binding,
                            template.target(),
                        ),
                        None => matches!(
                            template.target(),
                            crate::parser::DeferredForTarget::Unbound { .. }
                        ),
                    };
                candidate.then_some(index)
            })
            .collect()
    }
}

impl<S: Clone, P: PlanValueKind> AdmittedReplacementEffects<S, P> {
    pub fn dependency_analysis<'a>(
        &self,
        unresolved: impl IntoIterator<Item = &'a ResolvedResourceId>,
    ) -> Result<
        crate::effect::deps::DependencyAnalysis,
        crate::effect::deps::DependencyAnalysisFailure,
    > {
        crate::effect::deps::build_admitted_effect_dependency_analysis(self, unresolved)
    }
    pub(crate) fn order_edges(
        &self,
        deps: &mut HashMap<usize, HashSet<usize>>,
    ) -> HashSet<(usize, usize)> {
        let pairs = self.scheduling.order_edges(&self.effects, deps);
        for gate in 0..self.effects.len() {
            for delete in self.deferred_delete_candidates(gate) {
                deps.entry(delete).or_default().insert(gate);
            }
        }
        pairs
    }
    pub(crate) fn expand_deferred_replacements(self) -> (Self, HashSet<usize>, Vec<usize>) {
        let mut effects = Vec::with_capacity(self.effects.len());
        let mut deferred_replace_teardowns = HashSet::new();
        let mut source_effect_indices = Vec::with_capacity(self.effects.len());
        let mut remap = vec![None; self.effects.len()];
        for (source, effect) in self.effects.into_iter().enumerate() {
            if let Effect::DeferredReplace(payload) = &effect {
                let first_delete = effects.len();
                for old in payload.deletes.iter() {
                    effects.push(old.to_delete_effect());
                    source_effect_indices.push(source);
                }
                let gate = effects.len();
                deferred_replace_teardowns.extend(first_delete..gate);
            }
            remap[source] = Some(effects.len());
            effects.push(effect);
            source_effect_indices.push(source);
        }
        let mut scheduling = self.scheduling;
        scheduling.remap(&remap);
        (
            Self {
                effects,
                scheduling,
                temporary_names: self.temporary_names,
                writeback: self.writeback,
            },
            deferred_replace_teardowns,
            source_effect_indices,
        )
    }
}

/// Checked association used by checkpoint and final state writeback.
/// It is minted only from the owner of admitted replacement endpoints.
#[derive(Debug, Clone)]
pub struct ReplacementWriteback {
    target: ResolvedResourceId,
    deletes: NonEmptyDeletes,
}
impl ReplacementWriteback {
    fn from_checked<S, P: PlanValueKind>(
        checked: &CheckedReplacement<ResolvedResource<S, P>>,
    ) -> Self {
        Self {
            target: checked.target().resolved_id(),
            deletes: checked.old_instances().clone(),
        }
    }
    pub fn target(&self) -> &ResolvedResourceId {
        &self.target
    }
    pub fn old_instances(&self) -> &NonEmptyDeletes {
        &self.deletes
    }
    pub fn previous_attributes_for_state_writeback(
        &self,
        index: usize,
    ) -> crate::plan::ReplacementPreviousAttributesForStateWriteback<'_> {
        crate::plan::ReplacementPreviousAttributesForStateWriteback {
            attributes: &self.deletes[index].previous_attributes,
        }
    }
}

/// The value-free projection of a checked replacement. This is private to
/// admission and display; callers cannot schedule a separate effect vector.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplacementOrder {
    target: ResolvedResourceId,
    old_instances: Vec<crate::effect::DeletedInstanceKey>,
    kind: ReplacementOrderKind,
    /// Admission checks the resend order. Scheduling can select the separate
    /// poll-only branch only with an identified persisted continuation.
    continuation: Option<ContinuationOrder>,
}

#[derive(Debug, Clone, PartialEq)]
enum ReplacementOrderKind {
    DeleteFirst,
    CreateFirst,
}

#[derive(Debug, Clone, PartialEq)]
enum ContinuationOrder {
    Resend,
    Poll,
    Blocked,
}
impl ReplacementOrder {
    pub(crate) fn is_create_first(&self) -> bool {
        matches!(self.kind, ReplacementOrderKind::CreateFirst)
    }

    fn runs_before_cleanup(&self) -> bool {
        if matches!(
            self.continuation,
            Some(ContinuationOrder::Poll | ContinuationOrder::Blocked)
        ) {
            return true;
        }
        match self.kind {
            ReplacementOrderKind::DeleteFirst => false,
            ReplacementOrderKind::CreateFirst => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct BoundReplacement {
    order: ReplacementOrder,
    create: usize,
    deletes: Vec<usize>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ReplacementScheduling {
    replacements: Vec<BoundReplacement>,
    gate_barriers: HashMap<usize, HashSet<usize>>,
}
impl ReplacementScheduling {
    pub(crate) fn is_create_first(&self, target: &crate::resource::ResourceId) -> bool {
        self.replacements.iter().any(|replacement| {
            replacement.order.target.as_inner() == target && replacement.order.is_create_first()
        })
    }
    fn delete_slots<S, P: PlanValueKind>(
        order: &ReplacementOrder,
        effects: &[Effect<S, P>],
    ) -> Vec<usize> {
        effects
            .iter()
            .enumerate()
            .filter_map(|(index, effect)| match effect {
                Effect::Delete {
                    id,
                    identifier,
                    generation,
                    ..
                } if order
                    .old_instances
                    .contains(&crate::effect::DeletedInstanceKey::new(
                        id.as_inner().clone(),
                        generation.clone(),
                        identifier.as_str().to_owned(),
                    )) =>
                {
                    Some(index)
                }
                _ => None,
            })
            .collect()
    }
    fn insert<S, P: PlanValueKind>(
        &mut self,
        order: ReplacementOrder,
        effects: &[Effect<S, P>],
        create: usize,
    ) {
        let deletes = Self::delete_slots(&order, effects);
        self.replacements.push(BoundReplacement {
            order,
            create,
            deletes,
        });
    }
    pub(crate) fn extend_effects<S: Clone>(
        &mut self,
        checked: CheckedReplacement<ResolvedResource<S>>,
        consumer_updates: crate::resource::ResolvedDependencies,
        effects: &mut Vec<Effect<S>>,
    ) {
        let create = effects.len()
            + if checked.is_create_first() {
                0
            } else {
                checked.old_instances().len()
            };
        effects.extend(checked.clone().into_effects(consumer_updates));
        self.insert(checked.into_scheduling(), effects, create);
    }
    pub(crate) fn remap(&mut self, indices: &[Option<usize>]) {
        self.gate_barriers = self
            .gate_barriers
            .iter()
            .filter_map(|(delete, gates)| {
                let delete = indices.get(*delete).copied().flatten()?;
                let gates = gates
                    .iter()
                    .filter_map(|gate| indices.get(*gate).copied().flatten())
                    .collect();
                Some((delete, gates))
            })
            .collect();
        self.replacements.retain_mut(|replacement| {
            let Some(create) = indices.get(replacement.create).copied().flatten() else {
                return false;
            };
            replacement.create = create;
            replacement.deletes = replacement
                .deletes
                .iter()
                .filter_map(|old| indices.get(*old).copied().flatten())
                .collect();
            !replacement.deletes.is_empty()
        });
    }
    pub(crate) fn order_edges<S, P: PlanValueKind>(
        &self,
        effects: &[impl AsRef<Effect<S, P>>],
        deps: &mut HashMap<usize, HashSet<usize>>,
    ) -> HashSet<(usize, usize)> {
        for (&delete, gates) in &self.gate_barriers {
            deps.entry(delete).or_default().extend(gates);
        }
        let mut admitted_pairs = HashSet::new();
        for replacement in &self.replacements {
            let create = replacement.create;
            for &delete in &replacement.deletes {
                admitted_pairs.insert((create, delete));
                if !replacement.order.runs_before_cleanup() {
                    deps.entry(create).or_default().insert(delete);
                    continue;
                }
                deps.entry(delete).or_default().insert(create);
                // Parent lifetime windows use the same bound endpoints as pair
                // ordering. Each child's own admitted order stays independent.
                for (dependent, effect) in effects.iter().enumerate() {
                    if effect.as_ref().resource_id() != replacement.order.target.as_inner()
                        && matches!(
                            effect.as_ref(),
                            Effect::Create(_)
                                | Effect::ResumeCreate { .. }
                                | Effect::Update { .. }
                                | Effect::Read { .. }
                        )
                        && effect
                            .as_ref()
                            .blocking_keys()
                            .contains(&replacement.order.target)
                    {
                        deps.entry(delete).or_default().insert(dependent);
                        if let Some(child) = self.replacements.iter().find(|child| {
                            child.create == dependent && !child.order.runs_before_cleanup()
                        }) {
                            for &child_delete in &child.deletes {
                                deps.entry(child_delete).or_default().insert(create);
                            }
                        }
                    }
                }
            }
        }
        admitted_pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::EffectGeneration;
    use crate::effect::deps::{ScheduleInputs, build_effect_dependency_analysis};
    use crate::resource::{Resource, RootScope};
    use crate::schema::{PatternAttributeSchema, PatternAttributeType, PatternResourceSchema};

    fn string<P: crate::resource::ValueKind>(value: &str) -> Value<P> {
        Value::Concrete(ConcreteValue::String(value.into()))
    }
    fn fixture(create_first: bool) -> ReplacementRequest<ResolvedResource> {
        let mut target = Resource::new("test.Record", "record")
            .with_root_binding("record")
            .with_attribute("name", string("stable"));
        target.directives.create_before_destroy = create_first;
        let target = ResolvedResource::collect_resolved([target])
            .unwrap()
            .remove(0);
        let old = DeferredReplaceDelete {
            generation: crate::effect::EffectGeneration::Current,
            id: target.resolved_id(),
            identifier: crate::effect::fixture_applied_identifier("stable-physical-key"),
            directives: Default::default(),
            binding: Some(RootScope::new().resolve("record".into())),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
            previous_attributes: HashMap::from([("name".into(), string("stable"))]),
        };
        ReplacementRequest::new(target, NonEmptyDeletes::try_new(vec![old]).unwrap())
    }

    #[test]
    fn review10_4283_dependency_promotion_preserves_authored_directive() {
        for class in [
            UniqueNameSpec::Coexisting,
            UniqueNameSpec::Attribute("name".into()),
        ] {
            for authored in [false, true] {
                let schemas = registry(class.clone());
                let checked = resolve_replacement(
                    fixture(authored).require_create_first("consumer"),
                    &schemas,
                )
                .unwrap();
                assert!(checked.is_create_first());
                assert_eq!(checked.target().directives.create_before_destroy, authored);
                let readmitted = resolve_replacement(checked.into_request(), &schemas).unwrap();
                assert!(readmitted.is_create_first());
                assert_eq!(
                    readmitted.target().directives.create_before_destroy,
                    authored
                );
            }
        }
    }
    fn registry(class: UniqueNameSpec) -> SchemaRegistry {
        let schema = PatternResourceSchema::new("test.Record").attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string())
                .create_only()
                .build(),
        );
        let schema = match class {
            UniqueNameSpec::Coexisting => schema.with_coexisting_replacement(),
            UniqueNameSpec::Conflicting => schema.with_conflicting_replacement(),
            UniqueNameSpec::Attribute(attribute) => schema.with_unique_name_attribute(attribute),
        };
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema.validate().unwrap());
        registry
    }

    #[test]
    fn conflicting_replacement_orders_aliased_deposed_deletes_before_create() {
        // Historical provider aliases must preserve delete-before-create ordering,
        // both at the replacement's identity and at the source of a move.
        let schemas = registry(UniqueNameSpec::Conflicting);
        let mut unordered_cases = Vec::new();
        for (case, moved) in [("ordinary", false), ("moved", true)] {
            let request = fixture(false);
            let mut old = request.deletes[0].clone();
            let mut id = old.id.as_inner().clone();
            id.provider_instance = Some("old-route".into());
            if moved {
                id.set_identity("source".into());
            }
            old.id = ResolvedResourceId::new(id);
            old.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
            let mut plan = crate::plan::Plan::new();
            plan.add_delete(old);
            plan.add(Effect::Create(request.target().clone()));
            if moved {
                plan.add(Effect::Move {
                    from: ResolvedResourceId::with_identity("test.Record", "source"),
                    to: request.target().resolved_id(),
                    origin: None,
                });
            }
            let admitted = plan
                .admit_replacement_effects(&schemas)
                .expect("the aliased deposed replacement plan must be admitted");
            let graph = admitted
                .dependency_analysis(std::iter::empty())
                .expect("the replacement dependency graph must be acyclic");
            if !graph.deps_of(1).unwrap().contains(&0) {
                unordered_cases.push(case);
            }
        }
        assert!(
            unordered_cases.is_empty(),
            "deposed Delete must precede replacement Create for aliased generations; unordered cases: {unordered_cases:?}"
        );
    }

    fn deferred_deposed_secret_round_trip(deposed_attribute: &str, nested: bool) {
        let secret = |text| {
            Value::Deferred(crate::resource::DeferredValue::Secret(
                Box::new(string(text)).into(),
            ))
        };
        let mut old = fixture(false).deletes[0].clone();
        let current = crate::resource::SnapshotState::existing(
            old.id.as_inner().clone(),
            HashMap::from([("secret".into(), secret("current-secret"))]),
        );
        old.previous_attributes = current.attributes.clone();
        let current_guide = crate::value::redact_secrets_in_state(&current).unwrap();
        let deposed_value = if nested {
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "token".into(),
                secret("deposed-secret"),
            )])))
        } else {
            secret("deposed-secret")
        };
        let deposed_guide =
            crate::value::redact_secrets_in_state(&crate::resource::SnapshotState::existing(
                old.id.as_inner().clone(),
                HashMap::from([(deposed_attribute.into(), deposed_value)]),
            ))
            .unwrap();
        let mut deposed = old.clone();
        deposed.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        deposed.identifier = crate::effect::fixture_applied_identifier("deposed-id");
        deposed.previous_attributes = deposed_guide.attributes.clone();
        let template = crate::parser::ExpandedDeferredForExpression::new(
            None,
            1,
            "for value in upstream.items".into(),
            crate::parser::DeferredForTarget::bound(RootScope::new().resolve("record".into()))
                .unwrap(),
            Value::root_resource_ref("upstream", "items", vec![]),
            crate::parser::ForBinding::Simple("value".into()),
            Resource::new("test.Record", "record"),
        );
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::DeferredReplace(Box::new(
            crate::test_support::compiled_replace(
                NonEmptyDeletes::try_new(vec![old, deposed]).unwrap(),
                Box::new(template),
            ),
        )));
        let id = current.id.clone();
        let saved = crate::value::redact_secrets_in_plan_with_desired_and_historical::<()>(
            &plan,
            &HashMap::new(),
            &HashMap::from([(id.clone(), current_guide.clone())]),
        )
        .unwrap();
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains("current-secret"));
        assert!(!json.contains("deposed-secret\""));
        let wire: crate::plan::UnresolvedSavedPlan = serde_json::from_str(&json).unwrap();
        let saved = wire
            .resolve_with_schemas(&registry(UniqueNameSpec::Conflicting))
            .expect("saved replacement must resolve without changing generation evidence");
        let restored = crate::value::rehydrate_plan_historical_secrets_from_verified_states(
            &saved,
            &HashMap::from([(id.clone(), current_guide.clone())]),
            &HashMap::from([(id.clone(), current.clone())]),
        )
        .expect("deposed hashes must retain their own generation's evidence");
        let Effect::DeferredReplace(payload) = &restored.effects()[0] else {
            panic!("expected deferred replacement");
        };
        assert_eq!(payload.deletes[0].previous_attributes, current.attributes);
        assert_eq!(
            payload.deletes[1].previous_attributes,
            deposed_guide.attributes
        );

        let different =
            crate::value::redact_secrets_in_state(&crate::resource::SnapshotState::existing(
                id.clone(),
                HashMap::from([("secret".into(), secret("different-current-secret"))]),
            ))
            .unwrap();
        assert!(
            crate::value::rehydrate_plan_historical_secrets_from_verified_states(
                &saved,
                &HashMap::from([(id.clone(), different)]),
                &HashMap::from([(id, current)]),
            )
            .is_err(),
            "current-generation verification must still reject mismatched evidence",
        );
    }

    #[test]
    fn review11_4283_deferred_deposed_secret_hash_round_trip() {
        deferred_deposed_secret_round_trip("secret", false);
    }

    #[test]
    fn review11_4283_deferred_deposed_secret_shape_round_trip() {
        deferred_deposed_secret_round_trip("retired_secret", false);
    }

    #[test]
    fn review11_4283_deferred_deposed_secret_redaction_preserves_nested_shape() {
        deferred_deposed_secret_round_trip("secret", true);
    }

    #[test]
    fn review5_4283_moved_deposed_only_conflicting_replacement_is_rejected() {
        let request = fixture(true);
        let mut old = request.deletes[0].clone();
        old.id = ResolvedResourceId::with_provider_identity(
            "",
            "test.Record",
            "source",
            Some("source-route".into()),
        );
        old.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::Move {
            from: old.id.clone(),
            to: request.target().resolved_id(),
            origin: None,
        });
        plan.add_delete(old);
        plan.add(Effect::Create(request.target().clone()));
        let error = plan
            .prepare_replacements(&registry(UniqueNameSpec::Conflicting))
            .expect_err("a moved deposed generation still forbids create-first");
        assert!(matches!(
            error.cause,
            CreateBeforeDestroyError::ReplacementCannotCoexist(_)
        ));
    }

    #[test]
    fn review5_4283_moved_deposed_name_is_checked_without_rerouting_its_delete() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let mut request = fixture(true);
        request
            .deletes
            .iter_mut()
            .next()
            .unwrap()
            .previous_attributes = HashMap::from([("name".into(), string("current-B"))]);
        let mut deposed = request.deletes[0].clone();
        deposed.id = ResolvedResourceId::with_provider_identity(
            "",
            "test.Record",
            "source",
            Some("source-route".into()),
        );
        deposed.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        deposed.identifier = crate::effect::fixture_applied_identifier("source-deposed-A");
        deposed.previous_attributes = HashMap::from([("name".into(), string("stable"))]);
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::Move {
            from: deposed.id.clone(),
            to: request.target().resolved_id(),
            origin: None,
        });
        plan.add_delete(request.deletes[0].clone());
        plan.add_delete(deposed.clone());
        plan.add(Effect::Create(request.target().clone()));
        plan.prepare_replacements(&schemas).unwrap();
        let admitted = plan.admit_replacement_effects(&schemas).unwrap();
        assert_eq!(admitted.writeback()[0].old_instances().len(), 2);
        assert_eq!(admitted.writeback()[0].old_instances()[1], deposed);
        let prepared = &admitted.temporary_names()[0].1;
        assert_eq!(prepared.original_value, "stable");
        assert_ne!(prepared.temporary_value, "stable");
        assert_eq!(admitted.effects()[2], deposed.to_delete_effect());
        let graph = admitted.dependency_analysis(std::iter::empty()).unwrap();
        assert!(graph.deps_of(2).unwrap().contains(&3));
        let wire: crate::plan::UnresolvedSavedPlan =
            serde_json::from_value(serde_json::to_value(plan.to_snapshot()).unwrap()).unwrap();
        let loaded = wire.resolve_with_schemas(&schemas).unwrap();
        let loaded = loaded.admit_replacement_effects(&schemas).unwrap();
        assert_eq!(loaded.writeback()[0].old_instances()[1], deposed);
        assert_eq!(loaded.temporary_names(), admitted.temporary_names());
    }

    #[test]
    fn review5_4283_cycle_promotes_only_the_admissible_pair_with_timeout() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for class in [UniqueNameSpec::Coexisting, UniqueNameSpec::Conflicting] {
                let mut schemas = registry(class.clone());
                schemas.insert(
                    "",
                    PatternResourceSchema::new("test.Conflicting")
                        .with_conflicting_replacement()
                        .validate()
                        .unwrap(),
                );
                let mut coexisting = fixture(false);
                let mut conflicting = fixture(false).map_target(|target| {
                    let mut resource = target.as_resource().clone();
                    resource.id =
                        crate::resource::ResourceId::with_identity("test.Conflicting", "other");
                    resource.binding = Some(RootScope::new().resolve("other".into()));
                    ResolvedResource::from_resolved_parts(
                        resource,
                        Default::default(),
                        Default::default(),
                    )
                });
                let coexisting_id = coexisting.target().resolved_id();
                let conflicting_id = conflicting.target().resolved_id();
                conflicting.deletes.iter_mut().next().unwrap().id = conflicting_id.clone();
                coexisting
                    .deletes
                    .iter_mut()
                    .next()
                    .unwrap()
                    .blocked_by_updates = std::iter::once(conflicting_id.clone()).collect();
                conflicting
                    .deletes
                    .iter_mut()
                    .next()
                    .unwrap()
                    .blocked_by_updates = std::iter::once(coexisting_id.clone()).collect();
                let mut effects = vec![
                    coexisting.deletes[0].to_delete_effect(),
                    Effect::Create(coexisting.target().clone()),
                    conflicting.deletes[0].to_delete_effect(),
                    Effect::Create(conflicting.target().clone()),
                ];
                for _ in 0..2 {
                    let result =
                        AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new());
                    if matches!(class, UniqueNameSpec::Conflicting) {
                        let error = result
                            .expect_err("a cycle with only Conflicting pairs must be rejected");
                        assert!(error.required_by.is_some());
                        assert!(matches!(
                            error.cause,
                            CreateBeforeDestroyError::ReplacementCannotCoexist(_)
                        ));
                        effects.reverse();
                        continue;
                    }
                    let admitted = result.expect("one Coexisting promotion breaks this cycle");
                    assert!(
                        admitted
                            .scheduling
                            .is_create_first(coexisting_id.as_inner())
                    );
                    assert!(
                        !admitted
                            .scheduling
                            .is_create_first(conflicting_id.as_inner())
                    );
                    let graph = admitted.dependency_analysis(std::iter::empty()).unwrap();
                    let edges = graph.into_deps_of();
                    for (&node, dependencies) in &edges {
                        assert!(!dependencies.iter().any(|dependency| dependency_reaches(
                            &edges,
                            *dependency,
                            node
                        )));
                    }
                    effects.reverse();
                }
            }
            sender.send(()).unwrap();
        });
        // wall-clock-ok: A real deadline stops a synchronous promotion loop from hanging the test process.
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("promotion must finish within five seconds");
    }

    #[test]
    fn review5_4283_admission_cause_renders_once() {
        let request = fixture(true);
        let effects = vec![
            Effect::Create(request.target().clone()),
            request.deletes[0].to_delete_effect(),
        ];
        let error = AdmittedReplacementEffects::resolve(
            &effects,
            &registry(UniqueNameSpec::Conflicting),
            |_| HashMap::new(),
        )
        .unwrap_err();
        let rendered = crate::executor::render_error_chain(&error);
        assert_eq!(
            rendered.matches(&error.cause.to_string()).count(),
            1,
            "{rendered}"
        );
    }

    #[test]
    fn review5_4283_deferred_failure_preserves_the_admission_error_chain() {
        use std::error::Error;
        let schemas = registry(UniqueNameSpec::Conflicting);
        let request = fixture(false).require_create_first("consumer");
        let error = ReplacementAdmissionError {
            resource_id: Box::new(request.target().id.clone()),
            required_by: request.required_by().map(str::to_owned),
            cause: resolve_replacement(request.clone(), &schemas).unwrap_err(),
        };
        let failure = crate::executor::DeferredCreateFailure::Replacement { error };
        assert!(failure.to_string().contains("consumer"));
        let source = failure.source().unwrap();
        let admission = source.downcast_ref::<ReplacementAdmissionError>().unwrap();
        assert_eq!(admission.resource_id.as_ref(), &request.target().id);
        assert_eq!(admission.required_by.as_deref(), Some("consumer"));
        assert!(
            source
                .source()
                .unwrap()
                .downcast_ref::<CreateBeforeDestroyError>()
                .is_some()
        );
    }

    #[test]
    fn review4_4283_materialized_admission_uses_each_old_generations_snapshot() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let mut request = fixture(true);
        request
            .deletes
            .iter_mut()
            .next()
            .unwrap()
            .previous_attributes = HashMap::from([("name".into(), string("current-B"))]);
        let mut deposed = request.deletes[0].clone();
        deposed.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        deposed.identifier = crate::effect::fixture_applied_identifier("deposed-A");
        deposed.previous_attributes = HashMap::from([("name".into(), string("stable"))]);
        request.deletes =
            NonEmptyDeletes::try_new(vec![request.deletes[0].clone(), deposed]).unwrap();
        let effects: Vec<Effect> = request
            .deletes
            .iter()
            .map(|old| old.to_delete_effect())
            .collect();
        let existing =
            AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new()).unwrap();
        let admitted = existing
            .with_materialized(
                vec![Effect::Create(request.target().clone())],
                vec![request],
                &schemas,
            )
            .unwrap();
        let replacement = &admitted.writeback()[0];
        assert_eq!(
            replacement.old_instances()[0].previous_attributes["name"],
            string("current-B")
        );
        assert_eq!(
            replacement.old_instances()[1].previous_attributes["name"],
            string("stable")
        );
        let prepared = &admitted.temporary_names()[0].1;
        assert_eq!(prepared.original_value, "stable");
        assert_ne!(prepared.temporary_value, "stable");
        let readmitted = admitted
            .with_materialized(Vec::new(), Vec::new(), &schemas)
            .unwrap();
        assert_eq!(readmitted.temporary_names(), admitted.temporary_names());
        assert_eq!(
            readmitted.writeback()[0].old_instances(),
            replacement.old_instances()
        );
    }

    #[test]
    fn review4_4283_recovery_prepared_name_survives_without_display_metadata() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let request = fixture(true);
        let mut delete = request.deletes[0].to_delete_effect();
        if let Effect::Delete { generation, .. } = &mut delete {
            *generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        }
        let mut plan = crate::plan::Plan::new();
        plan.add(delete);
        plan.add(Effect::Create(request.target().clone()));
        plan.prepare_replacements(&schemas).unwrap();
        assert!(plan.replace_display_info().next().is_none());
        let expected = plan.effects()[1].clone();
        let original = plan.permanent_name_overrides[0].clone();
        assert_eq!(original.original_value.as_deref(), Some("stable"));
        plan.prepare_replacements(&schemas).unwrap();
        assert_eq!(
            plan.effects()[1],
            expected,
            "planning must reuse the prepared name"
        );
        let wire: crate::plan::UnresolvedSavedPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        let loaded = wire.resolve_with_schemas(&schemas).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded.effects()[1]).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(loaded.permanent_name_overrides, vec![original]);
        let execution = loaded.admit_replacement_effects(&schemas).unwrap();
        assert_eq!(execution.effects()[1], loaded.effects()[1]);
        assert_eq!(execution.temporary_names()[0].1.original_value, "stable");
    }

    #[test]
    fn review4_4283_plan_and_saved_plan_check_the_deposed_name() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let mut request = fixture(true);
        request
            .deletes
            .iter_mut()
            .next()
            .unwrap()
            .previous_attributes = HashMap::from([("name".into(), string("current-B"))]);
        let mut deposed = request.deletes[0].clone();
        deposed.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        deposed.identifier = crate::effect::fixture_applied_identifier("deposed-A");
        deposed.previous_attributes = HashMap::from([("name".into(), string("stable"))]);
        let checked = resolve_replacement(request, &schemas).unwrap();
        assert!(
            checked.temporary_name().is_none(),
            "A is distinct from current B"
        );
        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        plan.add_delete(deposed.clone());
        plan.prepare_replacements(&schemas).unwrap();
        let admitted = plan.admit_replacement_effects(&schemas).unwrap();
        assert_eq!(admitted.writeback()[0].old_instances()[1], deposed);
        let expected = admitted.temporary_names().clone();
        assert_eq!(expected[0].1.original_value, "stable");
        let dto = crate::plan::UnresolvedSavedPlan::from(crate::plan::SavedPlan::from(
            plan.to_snapshot(),
        ));
        let loaded = serde_json::from_str::<crate::plan::UnresolvedSavedPlan>(
            &serde_json::to_string(&dto).unwrap(),
        )
        .unwrap()
        .resolve_with_schemas(&schemas)
        .unwrap();
        let readmitted = loaded.admit_replacement_effects(&schemas).unwrap();
        assert_eq!(readmitted.temporary_names(), &expected);
        assert_eq!(
            readmitted.writeback()[0].old_instances(),
            admitted.writeback()[0].old_instances()
        );
        let mut request = ReplacementRequest::new(
            match &readmitted.effects()[0] {
                Effect::Create(target) => target.clone(),
                _ => panic!("expected create"),
            },
            readmitted.writeback()[0].old_instances().clone(),
        )
        .with_prepared_name(Some(expected[0].1.clone()));
        request
            .deletes
            .iter_mut()
            .nth(1)
            .unwrap()
            .previous_attributes
            .insert("name".into(), string(&expected[0].1.temporary_value));
        assert!(
            matches!(
                resolve_replacement(request, &schemas),
                Err(CreateBeforeDestroyError::InvalidPreparedReplacementName(_))
            ),
            "prepared names must be checked against the deposed generation too"
        );
    }

    #[test]
    fn saved_attribute_target_with_redacted_prior_name_survives_loading() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let request = fixture(true).map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.set_attr("name", string("approved-distinct-name"));
            target.replace_inner(resource)
        });
        let old = request.deletes[0].clone();
        let id = old.id.as_inner().clone();
        let verified =
            crate::resource::SnapshotState::existing(id.clone(), old.previous_attributes.clone());
        let guide =
            crate::value::redact_secrets_in_state(&crate::resource::SnapshotState::existing(
                id.clone(),
                HashMap::from([(
                    "name".into(),
                    Value::Deferred(crate::resource::DeferredValue::Secret(
                        Box::new(string("stable")).into(),
                    )),
                )]),
            ))
            .unwrap();
        let checked = resolve_replacement(request, &schemas).unwrap();
        assert!(checked.temporary_name().is_none());
        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        let guides = HashMap::from([(id.clone(), guide)]);
        let redacted = crate::value::redact_secrets_in_plan_with_desired_and_historical(
            &plan,
            &HashMap::<
                crate::resource::ResourceId,
                Resource<crate::resource::ModuleSource, crate::resource::SnapshotLeaf>,
            >::new(),
            &guides,
        )
        .unwrap();
        assert!(redacted.contains_redacted_secret_placeholders());
        let expected = serde_json::to_value(&redacted.effects()[0]).unwrap();
        let wire: crate::plan::UnresolvedSavedPlan =
            serde_json::from_value(serde_json::to_value(&redacted).unwrap()).unwrap();
        let loaded = wire.resolve_with_schemas(&schemas).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded.effects()[0]).unwrap(),
            expected
        );
        assert!(loaded.permanent_name_overrides.is_empty());
        let restored = crate::value::rehydrate_plan_historical_secrets_from_verified_states(
            &loaded,
            &guides,
            &HashMap::from([(id, verified)]),
        )
        .unwrap();
        let admitted = restored.admit(&schemas).unwrap();
        assert_eq!(
            serde_json::to_value(&admitted.effects()[0]).unwrap(),
            expected
        );
        assert!(admitted.permanent_name_overrides.is_empty());
    }

    #[test]
    fn review4_4283_generation_snapshots_preserve_their_own_secret_evidence() {
        let mut old = fixture(false).deletes[0].clone();
        let secret = |text| {
            Value::Deferred(crate::resource::DeferredValue::Secret(
                Box::new(string(text)).into(),
            ))
        };
        old.previous_attributes = HashMap::from([("secret".into(), secret("current-secret"))]);
        let current = crate::resource::SnapshotState::existing(
            old.id.as_inner().clone(),
            old.previous_attributes.clone(),
        );
        let current_guide = crate::value::redact_secrets_in_state(&current).unwrap();
        let deposed_guide =
            crate::value::redact_secrets_in_state(&crate::resource::SnapshotState::existing(
                old.id.as_inner().clone(),
                HashMap::from([("secret".into(), secret("deposed-secret"))]),
            ))
            .unwrap();
        let mut deposed = old.clone();
        deposed.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        deposed.identifier = crate::effect::fixture_applied_identifier("deposed-secret-id");
        deposed.previous_attributes = deposed_guide.attributes.clone();
        let id = old.id.as_inner().clone();
        let mut plan = crate::plan::Plan::new();
        plan.add_delete(old);
        plan.add_delete(deposed);
        let redacted = crate::value::redact_secrets_in_plan_with_desired_and_historical(
            &plan,
            &HashMap::<
                crate::resource::ResourceId,
                Resource<crate::resource::ModuleSource, crate::resource::SnapshotLeaf>,
            >::new(),
            &HashMap::from([(id.clone(), current_guide.clone())]),
        )
        .unwrap();
        assert!(redacted.contains_redacted_secret_placeholders());
        assert_eq!(
            redacted.replacement_inputs.old_instances[0].attributes,
            current_guide.attributes
        );
        assert_eq!(
            redacted.replacement_inputs.old_instances[1].attributes,
            deposed_guide.attributes
        );
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(!json.contains("current-secret"));
        assert!(!json.contains("deposed-secret\""));
        let wire: crate::plan::UnresolvedSavedPlan = serde_json::from_str(&json).unwrap();
        assert!(wire.contains_redacted_secret_placeholders());
        let restored = crate::value::rehydrate_plan_historical_secrets_from_verified_states(
            &redacted,
            &HashMap::from([(id.clone(), current_guide)]),
            &HashMap::from([(id, current.clone())]),
        )
        .unwrap();
        assert_eq!(
            restored.replacement_inputs.old_instances[0].attributes,
            current.attributes
        );
        assert_eq!(
            restored.replacement_inputs.old_instances[1].attributes,
            deposed_guide.attributes
        );
        let mut removed = restored;
        removed.retain(|_| false);
        assert!(removed.replacement_inputs.is_empty());
        assert!(!removed.contains_redacted_secret_placeholders());
    }

    #[test]
    fn admission_matrix_uses_the_targets_own_class_and_requirement() {
        for class in [
            UniqueNameSpec::Conflicting,
            UniqueNameSpec::Coexisting,
            UniqueNameSpec::Attribute("name".into()),
        ] {
            for (explicit, dependent) in [(false, false), (true, false), (false, true)] {
                let mut request = fixture(explicit);
                if dependent {
                    request = request.require_create_first("grandchild");
                }
                let result = resolve_replacement(request, &registry(class.clone()));
                if (explicit || dependent) && matches!(class, UniqueNameSpec::Conflicting) {
                    assert!(matches!(
                        result,
                        Err(CreateBeforeDestroyError::ReplacementCannotCoexist(_))
                    ));
                } else {
                    let admitted = result.unwrap();
                    assert_eq!(admitted.is_create_first(), explicit || dependent);
                    assert_eq!(
                        admitted.temporary_name().is_some(),
                        (explicit || dependent) && matches!(class, UniqueNameSpec::Attribute(_))
                    );
                    assert_eq!(
                        admitted.target().resolved_id(),
                        admitted.old_instances()[0].id
                    );
                }
            }
        }
    }

    #[test]
    fn attribute_naming_preserves_changed_name_and_prefix_rules() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let request = fixture(true).map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.set_attr("name", string("already-distinct"));
            target.replace_inner(resource)
        });
        assert!(
            resolve_replacement(request, &schemas)
                .unwrap()
                .temporary_name()
                .is_none()
        );
        let request = fixture(true).map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.prefixes.insert("name".into(), "prefix-".into());
            target.replace_inner(resource)
        });
        assert!(
            resolve_replacement(request, &schemas)
                .unwrap()
                .temporary_name()
                .is_none()
        );
    }

    #[test]
    fn checked_order_is_independent_of_endpoint_vector_position() {
        for create_first in [false, true] {
            let checked =
                resolve_replacement(fixture(create_first), &registry(UniqueNameSpec::Coexisting))
                    .unwrap();
            let mut effects = checked.into_effects(Default::default());
            for _ in 0..2 {
                effects.reverse();
                let create = effects
                    .iter()
                    .position(|effect| matches!(effect, Effect::Create(_)))
                    .unwrap();
                let delete = effects
                    .iter()
                    .position(|effect| matches!(effect, Effect::Delete { .. }))
                    .unwrap();
                let analysis = AdmittedReplacementEffects::resolve(
                    &effects,
                    &registry(UniqueNameSpec::Coexisting),
                    |_| HashMap::new(),
                )
                .unwrap()
                .dependency_analysis(std::iter::empty())
                .unwrap();
                let (after, before) = if create_first {
                    (delete, create)
                } else {
                    (create, delete)
                };
                assert!(analysis.deps_of(after).unwrap().contains(&before));
                assert!(!analysis.deps_of(before).unwrap().contains(&after));
            }
        }
    }

    #[test]
    fn raw_attribute_replacement_executes_the_prepared_target() {
        let request = fixture(true);
        let effects = vec![
            Effect::Create(request.target().clone()),
            request.old_instances()[0].to_delete_effect(),
        ];
        let admitted = AdmittedReplacementEffects::resolve(
            &effects,
            &registry(UniqueNameSpec::Attribute("name".into())),
            |_| request.old_instances()[0].previous_attributes.clone(),
        )
        .unwrap();
        let (effects, scheduling, names) = admitted.into_parts();
        let Effect::Create(target) = &effects[0] else {
            unreachable!()
        };
        assert_ne!(target.get_attr("name"), request.target().get_attr("name"));
        assert_eq!(names.len(), 1);
        assert!(scheduling.is_create_first(&target.id));
        assert_eq!(
            target.get_attr("name"),
            Some(&string(&names[0].1.temporary_value))
        );
    }

    #[test]
    fn raw_replacement_endpoints_cannot_bypass_admission() {
        let checked =
            resolve_replacement(fixture(false), &registry(UniqueNameSpec::Conflicting)).unwrap();
        let mut effects = checked.into_effects(Default::default());
        for _ in 0..2 {
            effects.reverse();
            let failure = build_effect_dependency_analysis(
                &effects,
                std::iter::empty(),
                ScheduleInputs::Apply,
            )
            .unwrap_err();
            assert_eq!(failure.unresolved().len(), 2);
        }
    }

    #[test]
    fn saved_plan_loading_rejects_unadmitted_conflicting_create_first() {
        let request = fixture(true);
        // Raw wire endpoints are intent, including if their vector says create-first.
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::Create(request.target().clone()));
        plan.add(request.old_instances()[0].to_delete_effect());
        let snapshot: crate::plan::SavedPlan = plan.to_snapshot().into();
        let dto = crate::plan::UnresolvedSavedPlan::from(snapshot);
        let decoded: crate::plan::UnresolvedSavedPlan =
            serde_json::from_str(&serde_json::to_string(&dto).unwrap()).unwrap();
        assert!(matches!(
            decoded
                .resolve_with_schemas(&registry(UniqueNameSpec::Conflicting))
                .unwrap()
                .admit(&registry(UniqueNameSpec::Conflicting))
                .map_err(|error| error.cause),
            Err(CreateBeforeDestroyError::ReplacementCannotCoexist(_))
        ));
    }
    #[test]
    fn saved_replacement_group_retains_its_old_instance_association() {
        let schemas = registry(UniqueNameSpec::Coexisting);
        let mut request = fixture(true);
        request.deletes.iter_mut().next().unwrap().id =
            ResolvedResourceId::with_identity("test.Record", "legacy-record");
        let checked = resolve_replacement(request, &schemas).unwrap();
        let target = checked.target().id.clone();
        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        let saved: crate::plan::SavedPlan = plan.to_snapshot().into();
        let dto = crate::plan::UnresolvedSavedPlan::from(saved);
        let loaded = dto
            .resolve_with_schemas(&schemas)
            .unwrap()
            .admit(&schemas)
            .unwrap();
        assert!(
            loaded
                .scheduling_effects()
                .scheduling
                .is_create_first(&target),
            "final admission must retain the differently addressed old instance"
        );
        assert!(
            loaded
                .replace_display_info()
                .next()
                .unwrap()
                .create_before_destroy
        );
    }

    #[test]
    fn checked_parent_window_contains_a_delete_first_child() {
        let mut schemas = registry(UniqueNameSpec::Conflicting);
        schemas.insert(
            "",
            PatternResourceSchema::new("test.Parent")
                .with_coexisting_replacement()
                .validate()
                .unwrap(),
        );
        let mut parent = fixture(true).map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.id = crate::resource::ResourceId::with_identity("test.Parent", "parent");
            resource.binding = Some(RootScope::new().resolve("parent".into()));
            ResolvedResource::from_resolved_parts(resource, Default::default(), Default::default())
        });
        parent.deletes.iter_mut().next().unwrap().id = parent.target().resolved_id();
        let parent = resolve_replacement(parent, &schemas).unwrap();
        let child = fixture(false).map_target(|target| {
            ResolvedResource::from_resolved_parts(
                target.as_resource().clone(),
                std::iter::once(parent.target().resolved_id()).collect(),
                Default::default(),
            )
        });
        let child = resolve_replacement(child, &schemas).unwrap();
        let parent_id = parent.target().resolved_id();
        let child_id = child.target().resolved_id();
        let mut effects = parent.into_effects(Default::default());
        effects.extend(child.into_effects(Default::default()));
        let create = |id: &ResolvedResourceId| {
            effects.iter().position(|effect| matches!(effect, Effect::Create(resource) if resource.resolved_id() == *id)).unwrap()
        };
        let delete = |id: &ResolvedResourceId| {
            effects
                .iter()
                .position(|effect| matches!(effect, Effect::Delete { id: old, .. } if old == id))
                .unwrap()
        };
        let graph = AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new())
            .unwrap()
            .dependency_analysis(std::iter::empty())
            .unwrap();
        assert!(
            graph
                .deps_of(delete(&child_id))
                .unwrap()
                .contains(&create(&parent_id)),
            "the old child must remain until its replacement parent exists"
        );
        assert!(
            graph
                .deps_of(create(&child_id))
                .unwrap()
                .contains(&delete(&child_id))
        );
        assert!(
            graph
                .deps_of(delete(&parent_id))
                .unwrap()
                .contains(&create(&child_id))
        );
    }
    #[test]
    fn saved_create_first_intent_cannot_be_downgraded_for_conflicting() {
        let request = fixture(false);
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::Create(request.target().clone()));
        plan.add(request.old_instances()[0].to_delete_effect());
        plan.replace_display
            .push(crate::plan::ReplaceDisplayMetadata {
                create_idx: 0,
                delete_idx: 1,
                create_before_destroy: true,
                changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                    .unwrap(),
                cascade_ref_hints: Vec::new(),
                temporary_name: None,
                previous_attributes: request.old_instances()[0].previous_attributes.clone(),
            });
        let saved: crate::plan::SavedPlan = plan.to_snapshot().into();
        let dto = crate::plan::UnresolvedSavedPlan::from(saved);
        assert!(matches!(
            dto.resolve_with_schemas(&registry(UniqueNameSpec::Conflicting))
                .unwrap()
                .admit(&registry(UniqueNameSpec::Conflicting))
                .map_err(|error| error.cause),
            Err(CreateBeforeDestroyError::ReplacementCannotCoexist(_))
        ));
    }
    #[test]
    fn review_4283_identity_proof_cannot_schedule_unprepared_attribute_create() {
        let request = fixture(true);
        let effects = vec![
            Effect::Create(request.target().clone()),
            request.old_instances()[0].to_delete_effect(),
        ];
        let admitted = AdmittedReplacementEffects::resolve(
            &effects,
            &registry(UniqueNameSpec::Attribute("name".into())),
            |_| request.old_instances()[0].previous_attributes.clone(),
        )
        .unwrap();
        let analysis =
            build_effect_dependency_analysis(&effects, std::iter::empty(), ScheduleInputs::Apply);
        assert!(
            analysis.is_err(),
            "raw same-name endpoints must remain unadmitted: {analysis:?}"
        );
        admitted.dependency_analysis(std::iter::empty()).unwrap();
        let Effect::Create(prepared) = &admitted.effects()[0] else {
            unreachable!()
        };
        assert_ne!(prepared.get_attr("name"), request.target().get_attr("name"));

        let checked = resolve_replacement(
            request.clone(),
            &registry(UniqueNameSpec::Attribute("name".into())),
        )
        .unwrap();
        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::Remove {
            id: ResolvedResourceId::with_identity("test.Record", "unrelated"),
        });
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        plan.retain(|effect| !matches!(effect, Effect::Remove { .. }));
        plan.dependency_analysis(std::iter::empty()).unwrap();
        let mapped = plan.clone().with_value_stage(effects);
        assert!(
            mapped.dependency_analysis(std::iter::empty()).is_err(),
            "mapping raw endpoints cannot inherit a scheduling proof"
        );
        plan.retain(|effect| !matches!(effect, Effect::Create(_)));
        plan.add(Effect::Create(request.target().clone()));
        assert!(
            plan.dependency_analysis(std::iter::empty()).is_err(),
            "retaining an old Delete cannot transfer admission to a raw Create"
        );
    }

    #[test]
    fn deferred_preflight_checks_reversed_historical_dependencies() {
        fn preview<S, P: PlanValueKind>(
            plan: crate::plan::AdmittedPlan<S, P>,
        ) -> Option<&'static str> {
            plan.effects().iter().find_map(|effect| match effect {
                Effect::DeferredReplace(payload) => Some(payload.display_glyph()),
                _ => None,
            })
        }

        for class in [UniqueNameSpec::Conflicting, UniqueNameSpec::Coexisting] {
            for reversed in [false, true] {
                for absorbed in [false, true] {
                    let mut schemas = registry(class.clone());
                    schemas.insert(
                        "",
                        PatternResourceSchema::new("test.Parent")
                            .with_coexisting_replacement()
                            .validate()
                            .unwrap(),
                    );
                    let mut parent = fixture(true).map_target(|target| {
                        let mut resource = target.as_resource().clone();
                        resource.id =
                            crate::resource::ResourceId::with_identity("test.Parent", "parent");
                        resource.binding = Some(RootScope::new().resolve("parent".into()));
                        ResolvedResource::new(resource)
                    });
                    let parent_id = parent.target().resolved_id();
                    parent.deletes.iter_mut().next().unwrap().id = parent_id.clone();
                    let mut old = fixture(false).deletes[0].clone();
                    old.id = ResolvedResourceId::with_provider_identity(
                        "",
                        "test.Record",
                        "records[0]",
                        None,
                    );
                    old.binding = Some(RootScope::new().resolve("records[0]".into()));
                    if reversed {
                        parent
                            .deletes
                            .iter_mut()
                            .next()
                            .unwrap()
                            .explicit_dependencies = std::iter::once(old.id.clone()).collect();
                    } else {
                        old.explicit_dependencies = std::iter::once(parent_id.clone()).collect();
                    }
                    let template = crate::parser::DeferredForExpression::new(
                        None,
                        1,
                        "for item in parent.values".into(),
                        crate::parser::DeferredForTarget::bound(
                            RootScope::new().resolve("records".into()),
                        )
                        .unwrap(),
                        Value::root_resource_ref("parent", "values", Vec::new()),
                        crate::parser::ForBinding::Simple("item".into()),
                        Resource::<_, crate::resource::LoopSlot>::new("test.Record", "records"),
                    )
                    .into_compiled();
                    let dependencies = crate::resource::DeferredDependencyTemplate::from_graph(
                        std::iter::once(parent_id.clone()).collect(),
                        Default::default(),
                        Default::default(),
                        Default::default(),
                        Default::default(),
                    );
                    let mut plan = crate::plan::Plan::new();
                    plan.add(Effect::Create(parent.target().clone()));
                    plan.add_delete(parent.deletes[0].clone());
                    let mut deposed = old.clone();
                    deposed.generation =
                        EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
                    deposed.identifier = crate::effect::fixture_applied_identifier("old-deposed");
                    if absorbed {
                        plan.add(Effect::DeferredReplace(Box::new(
                            crate::effect::DeferredReplacePayload::new(
                                NonEmptyDeletes::try_new(vec![old, deposed]).unwrap(),
                                Box::new(template),
                                dependencies,
                            ),
                        )));
                    } else {
                        plan.add_delete(old);
                        plan.add_delete(deposed);
                        plan.add(Effect::DeferredCreate(
                            crate::effect::DeferredCreatePayload::new(
                                Box::new(template),
                                dependencies,
                            ),
                        ));
                    }
                    for _ in 0..2 {
                        let wire: crate::plan::UnresolvedSavedPlan = serde_json::from_value(
                            serde_json::to_value(plan.to_snapshot()).unwrap(),
                        )
                        .unwrap();
                        for admission in [
                            plan.clone().admit(&schemas).map(preview),
                            wire.resolve_with_schemas(&schemas)
                                .unwrap()
                                .admit(&schemas)
                                .map(preview),
                        ] {
                            if reversed && matches!(class, UniqueNameSpec::Conflicting) {
                                let error = admission.expect_err(
                                    "known deferred cycles must fail before the parent can execute",
                                );
                                assert!(matches!(
                                    error.cause,
                                    CreateBeforeDestroyError::ReplacementCannotCoexist(_)
                                ));
                                assert!(error.required_by.is_some());
                            } else {
                                let glyph = admission.expect("an admissible deferred graph");
                                if absorbed {
                                    assert_eq!(glyph, Some(if reversed { "+/-" } else { "-/+" }));
                                }
                            }
                        }
                        plan.effects_mut().reverse();
                    }
                }
            }
        }
    }

    #[test]
    fn review_4283_reversed_historical_dependency_promotes_or_rejects() {
        for (class, reversed) in [
            (UniqueNameSpec::Coexisting, true),
            (UniqueNameSpec::Conflicting, true),
            (UniqueNameSpec::Conflicting, false),
        ] {
            let mut schemas = registry(class.clone());
            schemas.insert(
                "",
                PatternResourceSchema::new("test.Parent")
                    .with_coexisting_replacement()
                    .validate()
                    .unwrap(),
            );
            let mut a = fixture(true).map_target(|target| {
                let mut resource = target.as_resource().clone();
                resource.id = crate::resource::ResourceId::with_identity("test.Parent", "a");
                resource.binding = Some(RootScope::new().resolve("a".into()));
                ResolvedResource::from_resolved_parts(
                    resource,
                    Default::default(),
                    Default::default(),
                )
            });
            a.deletes.iter_mut().next().unwrap().id = a.target().resolved_id();
            let a_id = a.target().resolved_id();
            let b = fixture(false).map_target(|target| {
                ResolvedResource::from_resolved_parts(
                    target.as_resource().clone(),
                    Default::default(),
                    if reversed {
                        std::iter::once(a_id.clone()).collect()
                    } else {
                        Default::default()
                    },
                )
            });
            let b_id = b.target().resolved_id();
            a.deletes.iter_mut().next().unwrap().explicit_dependencies =
                std::iter::once(b_id.clone()).collect();
            let effects = vec![
                Effect::Create(a.target().clone()),
                a.deletes[0].to_delete_effect(),
                Effect::Create(b.target().clone()),
                b.deletes[0].to_delete_effect(),
            ];
            let admission =
                AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new());
            if !reversed {
                let (_, scheduling, _) = admission.unwrap().into_parts();
                assert!(
                    !scheduling.is_create_first(b_id.as_inner()),
                    "acyclic historical dependencies do not require coexistence"
                );
            } else if matches!(class, UniqueNameSpec::Conflicting) {
                assert!(
                    admission.is_err(),
                    "historical delete edges require an impossible B create-first"
                );
            } else {
                let (effects, scheduling, _) = admission.unwrap().into_parts();
                assert!(
                    scheduling.is_create_first(b_id.as_inner()),
                    "B must be promoted before scheduling the reversed old/new dependency"
                );
                let graph = AdmittedReplacementEffects::from_compiled_parts(
                    effects.clone(),
                    scheduling,
                    Vec::new(),
                    Vec::new(),
                )
                .dependency_analysis(std::iter::empty())
                .unwrap();
                let remaining = (0..effects.len()).collect();
                let dependencies = graph.into_deps_of();
                assert!(
                    crate::executor::classify_stalled_effects(
                        &effects,
                        &dependencies,
                        &remaining,
                        &HashSet::new()
                    )
                    .iter()
                    .all(|(_, failure)| !matches!(
                        failure,
                        crate::executor::EffectFailureKind::DependencyCycle { .. }
                    )),
                    "graph: {dependencies:?}"
                );
            }
        }
    }

    #[test]
    fn review_4283_redacted_plan_debug_drops_scheduling_snapshots() {
        let mut request = fixture(false);
        request
            .deletes
            .iter_mut()
            .next()
            .unwrap()
            .previous_attributes
            .insert("secret".into(), string("REVIEW_STATIC_SECRET_PLAINTEXT"));
        let id = request.target().id.clone();
        let guide = crate::value::redact_secrets_in_state(&crate::resource::State::<
            crate::resource::SnapshotLeaf,
        >::existing(
            id.clone(),
            HashMap::from([(
                "secret".into(),
                Value::Deferred(crate::resource::DeferredValue::Secret(
                    Box::new(string("REVIEW_STATIC_SECRET_PLAINTEXT")).into(),
                )),
            )]),
        ))
        .unwrap();
        let checked = resolve_replacement(request, &registry(UniqueNameSpec::Coexisting)).unwrap();
        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        let redacted = crate::value::redact_secrets_in_plan_with_desired_and_historical(
            &plan,
            &HashMap::<
                crate::resource::ResourceId,
                crate::resource::Resource<
                    crate::resource::ModuleSource,
                    crate::resource::SnapshotLeaf,
                >,
            >::new(),
            &HashMap::from([(id, guide)]),
        )
        .unwrap();
        assert!(
            !serde_json::to_string(&redacted)
                .unwrap()
                .contains("REVIEW_STATIC_SECRET_PLAINTEXT")
        );
        assert!(
            !format!("{redacted:?}").contains("REVIEW_STATIC_SECRET_PLAINTEXT"),
            "redacted plan Debug retained a plaintext scheduling snapshot"
        );
    }
    #[test]
    fn review2_4283_readmission_preserves_prepared_name_without_prior_name() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        for redacted in [false, true] {
            let mut request = fixture(true);
            request
                .deletes
                .iter_mut()
                .next()
                .unwrap()
                .previous_attributes
                .clear();
            if redacted {
                let secret: SnapshotValue = Value::Deferred(
                    crate::resource::DeferredValue::Secret(Box::new(string("stable")).into()),
                );
                let state = crate::resource::State::existing(
                    request.target().id.clone(),
                    HashMap::from([("name".into(), secret)]),
                );
                let value = crate::value::redact_secrets_in_state(&state)
                    .unwrap()
                    .attributes
                    .remove("name")
                    .unwrap();
                request
                    .deletes
                    .iter_mut()
                    .next()
                    .unwrap()
                    .previous_attributes
                    .insert("name".into(), value);
            }
            let checked = resolve_replacement(request, &schemas).unwrap();
            let expected_name = checked.temporary_name().unwrap().clone();
            let checked = resolve_replacement(checked.into_request(), &schemas).unwrap();
            assert_eq!(
                checked.temporary_name(),
                Some(&expected_name),
                "re-admission must retain the original DSL name and prepared suffix"
            );
            let mut plan = crate::plan::Plan::new();
            plan.add_replacement(crate::plan::ReplacementGroup {
                replacement: checked,
                changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                    .unwrap(),
                cascade_ref_hints: Vec::new(),
                consumer_updates: Default::default(),
            });
            plan.prepare_replacements(&schemas).unwrap();
            let wire: crate::plan::UnresolvedSavedPlan =
                serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
            let loaded = wire.resolve_with_schemas(&schemas).unwrap();
            let prepared = loaded.admit_replacement_effects(&schemas).unwrap();
            assert_eq!(prepared.temporary_names()[0].1, expected_name);
            assert_eq!(
                loaded.replace_display_info().next().unwrap().temporary_name,
                Some(&expected_name)
            );
            assert_eq!(
                loaded.permanent_name_overrides[0].original_value.as_deref(),
                Some("stable")
            );
        }
    }

    #[test]
    fn review2_4283_deposed_create_cannot_bypass_admission() {
        let request = fixture(true);
        let mut delete = request.deletes[0].to_delete_effect();
        if let Effect::Delete { generation, .. } = &mut delete {
            *generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        }
        let effects = vec![Effect::Create(request.target().clone()), delete];
        assert!(
            AdmittedReplacementEffects::resolve(
                &effects,
                &registry(UniqueNameSpec::Conflicting),
                |_| HashMap::new()
            )
            .is_err(),
            "a recovery generation still requires the target's coexistence admission"
        );
    }

    #[test]
    fn resume_cleanup_barrier_does_not_request_conflicting_replacement() {
        for moved in [false, true] {
            let request = fixture(false);
            let mut old = request.deletes[0].clone();
            old.generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
            old.blocked_by_updates = [request.target().resolved_id()].into_iter().collect();
            let mut effects = vec![Effect::ResumeCreate {
                resource: request.target().clone(),
                started_at: crate::provider::Timestamp::now(),
            }];
            if moved {
                old.id = ResolvedResourceId::with_identity("test.Record", "source");
                effects.push(Effect::Move {
                    from: old.id.clone(),
                    to: request.target().resolved_id(),
                    origin: None,
                });
            }
            let delete = effects.len();
            effects.push(old.to_delete_effect());
            let schemas = registry(UniqueNameSpec::Conflicting);
            let admitted =
                AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new())
                    .expect("resuming an issued create must retain its cleanup barrier")
                    .schedule(&HashMap::from([(
                        request.target().id.clone(),
                        persisted_identified_create(request.target()),
                    )]));
            assert_eq!(admitted.effects()[0], effects[0]);
            assert_eq!(admitted.writeback()[0].old_instances().len(), 1);
            let graph = admitted.dependency_analysis(std::iter::empty()).unwrap();
            assert!(graph.deps_of(delete).unwrap().contains(&0));
            assert!(!graph.deps_of(0).is_some_and(|deps| deps.contains(&delete)));
            let mut plan = crate::plan::Plan::new();
            for effect in effects {
                plan.add(effect);
            }
            let wire: crate::plan::UnresolvedSavedPlan =
                serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
            let loaded = wire.resolve_with_schemas(&schemas).unwrap();
            let loaded = loaded.admit_replacement_effects(&schemas).unwrap();
            assert_eq!(
                serde_json::to_value(&loaded.effects()[0]).unwrap(),
                serde_json::to_value(&admitted.effects()[0]).unwrap()
            );
            assert_eq!(loaded.writeback()[0].old_instances().len(), 1);
        }
    }

    #[test]
    fn review3_4283_resume_create_preserves_issued_attribute_name() {
        let request = fixture(true).map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.set_attr("name", string("already-issued-name"));
            target.replace_inner(resource)
        });
        let mut delete = request.deletes[0].to_delete_effect();
        if let Effect::Delete { generation, .. } = &mut delete {
            *generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        }
        let effects = vec![
            Effect::ResumeCreate {
                resource: request.target().clone(),
                started_at: crate::provider::Timestamp::now(),
            },
            delete,
        ];
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let admitted =
            AdmittedReplacementEffects::resolve(&effects, &schemas, |_| HashMap::new()).unwrap();
        let Effect::ResumeCreate { resource, .. } = &admitted.effects()[0] else {
            panic!("admission must retain the continuation");
        };
        assert_eq!(
            resource.get_attr("name"),
            Some(&string("already-issued-name"))
        );
        assert!(
            admitted.temporary_names().is_empty(),
            "a continuation must not invent a name override"
        );
        let mut plan = crate::plan::Plan::new();
        for effect in effects {
            plan.add(effect);
        }
        plan.prepare_replacements(&schemas).unwrap();
        assert!(plan.permanent_name_overrides.is_empty());
        let readmitted = plan.admit_replacement_effects(&schemas).unwrap();
        assert!(readmitted.temporary_names().is_empty());
        assert_eq!(readmitted.effects()[0], admitted.effects()[0]);
    }

    #[test]
    fn review2_4283_resume_create_cannot_bypass_admission() {
        for deposed in [false, true] {
            let request = fixture(true);
            let mut delete = request.deletes[0].to_delete_effect();
            if deposed && let Effect::Delete { generation, .. } = &mut delete {
                *generation = EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
            }
            let effects = vec![
                Effect::ResumeCreate {
                    resource: request.target().clone(),
                    started_at: crate::provider::Timestamp::now(),
                },
                delete,
            ];
            assert!(
                AdmittedReplacementEffects::resolve(
                    &effects,
                    &registry(UniqueNameSpec::Conflicting),
                    |_| HashMap::new()
                )
                .is_err(),
                "a continuation still requires replacement admission"
            );
        }
    }

    #[test]
    fn review2_4283_direct_deserialization_cannot_mint_displayable_plan() {
        let checked =
            resolve_replacement(fixture(true), &registry(UniqueNameSpec::Coexisting)).unwrap();
        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: checked,
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["name".into()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            consumer_updates: Default::default(),
        });
        let json = serde_json::to_value(&plan).unwrap();
        let wire: crate::plan::UnresolvedSavedPlan = serde_json::from_value(json).unwrap();
        assert!(
            wire.resolve_with_schemas(&registry(UniqueNameSpec::Conflicting))
                .unwrap()
                .admit(&registry(UniqueNameSpec::Conflicting))
                .is_err()
        );
    }

    #[test]
    fn review2_4283_prepared_name_is_revalidated_against_target_and_class() {
        let schemas = registry(UniqueNameSpec::Attribute("name".into()));
        let checked = resolve_replacement(fixture(true), &schemas).unwrap();
        assert!(
            resolve_replacement(
                checked.clone().into_request(),
                &registry(UniqueNameSpec::Conflicting)
            )
            .is_err()
        );
        let mut colliding = checked.clone().into_request();
        colliding
            .deletes
            .iter_mut()
            .next()
            .unwrap()
            .previous_attributes
            .insert(
                "name".into(),
                string(&checked.temporary_name().unwrap().temporary_value),
            );
        assert!(matches!(
            resolve_replacement(colliding, &schemas),
            Err(CreateBeforeDestroyError::InvalidPreparedReplacementName(_))
        ));
        let tampered = checked.into_request().map_target(|target| {
            let mut resource = target.as_resource().clone();
            resource.set_attr("name", string("changed-after-preparation"));
            target.replace_inner(resource)
        });
        assert!(matches!(
            resolve_replacement(tampered, &schemas),
            Err(CreateBeforeDestroyError::InvalidPreparedReplacementName(_))
        ));
    }

    #[test]
    fn review2_4283_recovery_pairs_admit_every_generation_and_continuation() {
        for resume in [false, true] {
            for create_first in [false, true] {
                let request = fixture(create_first);
                let target = if resume {
                    Effect::ResumeCreate {
                        resource: request.target().clone(),
                        started_at: crate::provider::Timestamp::now(),
                    }
                } else {
                    Effect::Create(request.target().clone())
                };
                let mut first = request.deletes[0].to_delete_effect();
                let mut second = first.clone();
                if let Effect::Delete { generation, .. } = &mut first {
                    *generation =
                        EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
                }
                if let Effect::Delete { generation, .. } = &mut second {
                    *generation =
                        EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
                }
                let effects = vec![first, target, second, request.deletes[0].to_delete_effect()];
                assert!(
                    build_effect_dependency_analysis(
                        &effects,
                        std::iter::empty(),
                        ScheduleInputs::Apply
                    )
                    .is_err()
                );
                let admitted = AdmittedReplacementEffects::resolve(
                    &effects,
                    &registry(UniqueNameSpec::Coexisting),
                    |_| HashMap::new(),
                )
                .unwrap()
                .schedule(&if resume {
                    HashMap::from([(
                        request.target().id.clone(),
                        persisted_identified_create(request.target()),
                    )])
                } else {
                    HashMap::new()
                });
                let graph = admitted.dependency_analysis(std::iter::empty()).unwrap();
                for delete in [0, 2, 3] {
                    // An issued create resumes before cleanup regardless of
                    // the policy for issuing a fresh replacement.
                    let (after, before) = if resume || create_first {
                        (delete, 1)
                    } else {
                        (1, delete)
                    };
                    assert!(graph.deps_of(after).unwrap().contains(&before));
                    assert!(!graph.deps_of(before).unwrap().contains(&after));
                }
            }
        }
    }

    fn persisted_identified_create(target: &ResolvedResource) -> crate::provider::InFlightCreate {
        let (attempt, _cancellation) = crate::provider::CreateAttempt::new(target.id.clone());
        attempt
            .issued(
                crate::provider::AppliedIdentifier::new("new-identifier").unwrap(),
                crate::provider::Sent::AllRequestAttributes,
            )
            .pending(crate::provider::PendingToken::new("continuation"), 1)
            .persist(Default::default(), Default::default())
    }
}
