//! Replacement intent and the sole admission boundary for replacement order.
//!
//! Persisted plans and deferred templates carry requests, never admission proofs.
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::effect::{DeferredReplaceDelete, Effect, NonEmptyDeletes, TemporaryName};
use crate::plan::{
    CreateBeforeDestroyError, ReplacementCannotCoexistError, SchemaNotRegisteredError,
};
use crate::resource::{
    ConcreteValue, PlanValueKind, ResolvedResource, ResolvedResourceId, SnapshotValue, Value,
};
use crate::schema::{SchemaKind, SchemaRegistry, UniqueNameSpec};

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
}
impl<T> ReplacementRequest<T> {
    pub fn new(target: T, old_instances: NonEmptyDeletes) -> Self {
        Self {
            template: target,
            deletes: old_instances,
            required_by: None,
            prepared_name: None,
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
                .map(|old| {
                    crate::effect::DeletedInstanceKey::new(
                        old.id.as_inner().clone(),
                        old.generation.clone(),
                        old.identifier.as_str().to_owned(),
                    )
                })
                .collect(),
            create_first: self.is_create_first(),
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
        )?;
        let mut resource = target.as_resource().clone();
        resource.directives.create_before_destroy = true;
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
    })
}

fn prepare_name<S, P: PlanValueKind>(
    target: &ResolvedResource<S, P>,
    old: &NonEmptyDeletes,
    schema: &crate::schema::CompiledResourceSchema,
    prepared: Option<&TemporaryName>,
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

/// New consumers require coexistence. Old delete edges can additionally force
/// promotion when the old and new dependency directions form a scheduling cycle.
pub(crate) fn create_first_requirements<S, P: PlanValueKind>(
    effects: &[Effect<S, P>],
) -> HashMap<ResolvedResourceId, String> {
    replacement_requirements(effects, &HashMap::new(), &HashMap::new())
}

fn replacement_requirements<S, P: PlanValueKind>(
    effects: &[Effect<S, P>],
    associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
    required: &HashMap<ResolvedResourceId, String>,
) -> HashMap<ResolvedResourceId, String> {
    let new_dependencies: Vec<crate::resource::ResolvedDependencies> = effects
        .iter()
        .map(|effect| match effect {
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.all_dependencies().cloned().collect()
            }
            Effect::Update { to, .. } => to.all_dependencies().cloned().collect(),
            Effect::DeferredCreate(payload) => payload.dependency_template.dependencies(),
            Effect::DeferredReplace(payload) => payload.dependency_template.dependencies(),
            Effect::Read { .. }
            | Effect::Delete { .. }
            | Effect::Wait { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Default::default(),
        })
        .collect();
    let mut requirements = required.clone();
    for (effect, dependencies) in effects.iter().zip(&new_dependencies) {
        for dependency in dependencies.iter() {
            if dependency.as_inner() != effect.resource_id() {
                requirements
                    .entry(dependency.clone())
                    .or_insert_with(|| effect.resource_id().to_string());
            }
        }
    }
    let creates: HashMap<_, _> = effects
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| match effect {
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                Some((resource.resolved_id(), index))
            }
            _ => None,
        })
        .collect();
    let deletes: Vec<_> = effects
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| match effect {
            Effect::Delete { id, .. } => Some((id.clone(), index)),
            _ => None,
        })
        .collect();
    let pairs: Vec<_> = creates
        .iter()
        .flat_map(|(target, &create)| {
            deletes
                .iter()
                .filter(move |(old, _)| {
                    old == target
                        || associations
                            .get(target)
                            .is_some_and(|ids| ids.contains(old))
                })
                .map(move |(_, delete)| (target.clone(), create, *delete))
        })
        .collect();
    loop {
        let create_first = |target: &ResolvedResourceId, index: usize| {
            matches!(&effects[index], Effect::Create(resource) | Effect::ResumeCreate { resource, .. } if resource.directives.create_before_destroy)
                || requirements.contains_key(target)
        };
        let mut edges: HashMap<usize, HashSet<usize>> = HashMap::new();
        for (index, dependencies) in new_dependencies.iter().enumerate() {
            for dependency in dependencies.iter() {
                if let Some(&upstream) = creates.get(dependency) {
                    edges.entry(index).or_default().insert(upstream);
                }
            }
        }
        for &(_, delete) in &deletes {
            if let Effect::Delete {
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
                ..
            } = &effects[delete]
            {
                let mut old_dependencies = dependencies.to_resolved_dependencies();
                old_dependencies.extend(explicit_dependencies.iter().cloned());
                for old_dependency in old_dependencies {
                    for (_, upstream_delete) in
                        deletes.iter().filter(|(id, _)| id == &old_dependency)
                    {
                        edges.entry(*upstream_delete).or_default().insert(delete);
                    }
                }
                for consumer in blocked_by_updates.iter() {
                    if let Some(&consumer) = creates.get(consumer) {
                        edges.entry(delete).or_default().insert(consumer);
                    }
                }
            }
        }
        for (target, create, delete) in &pairs {
            if create_first(target, *create) {
                edges.entry(*delete).or_default().insert(*create);
                for (consumer, dependencies) in new_dependencies.iter().enumerate() {
                    if consumer == *create || !dependencies.contains(target) {
                        continue;
                    }
                    edges.entry(*delete).or_default().insert(consumer);
                    for (child, child_create, child_delete) in &pairs {
                        if *child_create == consumer && !create_first(child, consumer) {
                            edges.entry(*child_delete).or_default().insert(*create);
                        }
                    }
                }
            } else {
                edges.entry(*create).or_default().insert(*delete);
            }
        }
        let promotions: Vec<_> = pairs
            .iter()
            .filter(|(target, create, delete)| {
                !create_first(target, *create) && dependency_reaches(&edges, *delete, *create)
            })
            .map(|(target, create, delete)| {
                let dependent = edges
                    .get(delete)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|candidate| {
                        effects[*candidate].resource_id() != target.as_inner()
                            && dependency_reaches(&edges, *candidate, *create)
                    })
                    .min()
                    .map(|index| effects[index].resource_id().to_string())
                    .unwrap_or_else(|| target.to_string());
                (target.clone(), dependent)
            })
            .collect();
        if promotions.is_empty() {
            break;
        }
        requirements.extend(promotions);
    }
    requirements
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
#[error("{cause}")]
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

impl<S: Clone, P: PlanValueKind> AdmittedReplacementEffects<S, P> {
    pub fn resolve(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
    ) -> Result<Self, ReplacementAdmissionError> {
        Self::resolve_with_associations(effects, registry, previous, &HashMap::new())
    }
    pub(crate) fn resolve_with_associations(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
        associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
    ) -> Result<Self, ReplacementAdmissionError> {
        Self::resolve_with_prepared_names(
            effects,
            registry,
            previous,
            associations,
            &HashMap::new(),
        )
    }
    pub(crate) fn resolve_with_prepared_names(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
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
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
        associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
        prepared_names: &HashMap<ResolvedResourceId, TemporaryName>,
        required: &HashMap<ResolvedResourceId, String>,
    ) -> Result<Self, ReplacementAdmissionError> {
        let mut effects = effects.to_vec();
        let requirements = replacement_requirements(&effects, associations, required);
        let mut scheduling = ReplacementScheduling::default();
        let mut temporary_names = Vec::new();
        let mut writeback = Vec::new();
        for index in 0..effects.len() {
            if let Effect::DeferredReplace(payload) = &mut effects[index] {
                payload.require_create_first_for_dependencies(&requirements);
                payload
                    .preflight(registry)
                    .map_err(|cause| ReplacementAdmissionError {
                        resource_id: Box::new(payload.id().as_inner().clone()),
                        required_by: payload.request.required_by().map(str::to_owned).or_else(
                            || {
                                payload
                                    .required_children
                                    .first()
                                    .map(|(_, dependent)| dependent.clone())
                            },
                        ),
                        cause,
                    })?;
            }
            let target = match &effects[index] {
                Effect::Create(target)
                | Effect::ResumeCreate {
                    resource: target, ..
                } => target,
                _ => continue,
            };
            let old: Vec<_> = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Delete {
                        id,
                        identifier,
                        generation,
                        directives,
                        binding,
                        dependencies,
                        explicit_dependencies,
                        blocked_by_updates,
                    } if target.id == **id
                        || associations
                            .get(&target.resolved_id())
                            .is_some_and(|old| old.contains(id)) =>
                    {
                        Some(DeferredReplaceDelete {
                            generation: generation.clone(),
                            id: id.clone(),
                            identifier: identifier.clone(),
                            directives: directives.clone(),
                            binding: binding.clone(),
                            dependencies: dependencies.clone(),
                            explicit_dependencies: explicit_dependencies.clone(),
                            blocked_by_updates: blocked_by_updates.clone(),
                            previous_attributes: previous(&target.resolved_id()),
                        })
                    }
                    _ => None,
                })
                .collect();
            if let Ok(old) = NonEmptyDeletes::try_new(old) {
                let mut request = ReplacementRequest::new(target.clone(), old)
                    .with_prepared_name(prepared_names.get(&target.resolved_id()).cloned());
                if let Some(dependent) = requirements.get(&target.resolved_id()) {
                    request = request.require_create_first(dependent);
                }
                let resource_id = Box::new(request.target().id.clone());
                let required_by = request.required_by().map(str::to_owned);
                let checked = resolve_replacement(request, registry).map_err(|cause| {
                    ReplacementAdmissionError {
                        resource_id,
                        required_by,
                        cause,
                    }
                })?;
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
        }
        Ok(Self {
            effects,
            scheduling,
            temporary_names,
            writeback,
        })
    }
    pub fn effects(&self) -> &[Effect<S, P>] {
        &self.effects
    }
    pub(super) fn from_compiled_parts(
        effects: Vec<Effect<S, P>>,
        scheduling: ReplacementScheduling,
        temporary_names: PreparedReplacementNames,
    ) -> Self {
        Self {
            effects,
            scheduling,
            temporary_names,
            writeback: Vec::new(),
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
            previous.insert(
                replacement.target.clone(),
                replacement.deletes[0].previous_attributes.clone(),
            );
        }
        for request in requests {
            let target = request.target().resolved_id();
            associations
                .entry(target.clone())
                .or_default()
                .extend(request.deletes.iter().map(|old| old.id.clone()));
            previous.insert(
                target.clone(),
                request.deletes[0].previous_attributes.clone(),
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

impl<S: Clone> AdmittedReplacementEffects<S> {
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
        self.scheduling.order_edges(&self.effects, deps)
    }
    pub(crate) fn expand_deferred_replacements(self) -> (Self, Vec<(usize, usize)>, Vec<usize>) {
        let expanded = crate::executor::parallel::expand_deferred_replace_effects(&self.effects);
        let mut remap = vec![None; self.effects.len()];
        for (index, source) in expanded.source_effect_indices.iter().enumerate() {
            remap[*source] = Some(index);
        }
        let mut scheduling = self.scheduling;
        scheduling.remap(&remap);
        (
            Self {
                effects: expanded.effects,
                scheduling,
                temporary_names: self.temporary_names,
                writeback: self.writeback,
            },
            expanded.deferred_replace_delete_deps,
            expanded.source_effect_indices,
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
    create_first: bool,
}
impl ReplacementOrder {
    pub(crate) fn is_create_first(&self) -> bool {
        self.create_first
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
    pub(crate) fn order_edges<S>(
        &self,
        effects: &[impl AsRef<Effect<S>>],
        deps: &mut HashMap<usize, HashSet<usize>>,
    ) -> HashSet<(usize, usize)> {
        let mut admitted_pairs = HashSet::new();
        for replacement in &self.replacements {
            let create = replacement.create;
            for &delete in &replacement.deletes {
                admitted_pairs.insert((create, delete));
                if !replacement.order.is_create_first() {
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
                            child.create == dependent && !child.order.is_create_first()
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
            decoded.resolve_with_schemas(&registry(UniqueNameSpec::Conflicting)),
            Err(crate::resource::ResolvedResourceError::Replacement(
                CreateBeforeDestroyError::ReplacementCannotCoexist(_)
            ))
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
            permanent_name_override: None,
            consumer_updates: Default::default(),
        });
        let saved: crate::plan::SavedPlan = plan.to_snapshot().into();
        let dto = crate::plan::UnresolvedSavedPlan::from(saved);
        let loaded = dto.resolve_with_schemas(&schemas).unwrap();
        assert!(
            loaded
                .scheduling_effects()
                .scheduling
                .is_create_first(&target),
            "loading must re-admit the group, including a differently addressed old instance"
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
            dto.resolve_with_schemas(&registry(UniqueNameSpec::Conflicting)),
            Err(crate::resource::ResolvedResourceError::Replacement(
                CreateBeforeDestroyError::ReplacementCannotCoexist(_)
            ))
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
            permanent_name_override: None,
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
            permanent_name_override: None,
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
                permanent_name_override: None,
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
            permanent_name_override: None,
            consumer_updates: Default::default(),
        });
        let json = serde_json::to_value(&plan).unwrap();
        let wire: crate::plan::UnresolvedSavedPlan = serde_json::from_value(json).unwrap();
        assert!(
            wire.resolve_with_schemas(&registry(UniqueNameSpec::Conflicting))
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
                .unwrap();
                let graph = admitted.dependency_analysis(std::iter::empty()).unwrap();
                for delete in [0, 2, 3] {
                    let (after, before) = if create_first {
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
}
