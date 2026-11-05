//! Replacement intent and the sole admission boundary for replacement order.
//!
//! Persisted plans and deferred templates carry requests, never admission proofs.
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::effect::{
    DeferredReplaceDelete, Effect, EffectGeneration, NonEmptyDeletes, TemporaryName,
};
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
}
impl<T> ReplacementRequest<T> {
    pub fn new(target: T, old_instances: NonEmptyDeletes) -> Self {
        Self {
            template: target,
            deletes: old_instances,
            required_by: None,
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
    pub fn require_create_first(mut self, dependent: impl Into<String>) -> Self {
        self.required_by = Some(dependent.into());
        self
    }
    pub fn map_target<U>(self, map: impl FnOnce(T) -> U) -> ReplacementRequest<U> {
        ReplacementRequest {
            template: map(self.template),
            deletes: self.deletes,
            required_by: self.required_by,
        }
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
        let target = match self.admitted {
            Admitted::DeleteFirst(target) => target,
            Admitted::CreateFirst(witness) => witness.target,
        };
        ReplacementRequest {
            template: target,
            deletes: self.old_instances,
            required_by: self.required_by,
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
    fn map_target<U>(self, map: impl FnOnce(T) -> U) -> CheckedReplacement<U> {
        let admitted = match self.admitted {
            Admitted::DeleteFirst(target) => Admitted::DeleteFirst(map(target)),
            Admitted::CreateFirst(witness) => Admitted::CreateFirst(CreateFirst {
                target: map(witness.target),
                temporary_name: witness.temporary_name,
            }),
        };
        CheckedReplacement {
            admitted,
            old_instances: self.old_instances,
            required_by: self.required_by,
        }
    }
}

impl<S, P: PlanValueKind> CheckedReplacement<ResolvedResource<S, P>> {
    pub(crate) fn into_scheduling(self) -> CheckedReplacement<ResolvedResourceId> {
        self.map_target(|target| target.resolved_id())
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
        let temporary_name = prepare_name(&target, &old_instances, schema.as_schema())?;
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

/// Creation dependencies request coexistence; historical delete edges do not.
pub(crate) fn create_first_requirements<S, P: PlanValueKind>(
    effects: &[Effect<S, P>],
) -> HashMap<ResolvedResourceId, String> {
    let mut requirements = HashMap::new();
    for effect in effects {
        let dependencies: crate::resource::ResolvedDependencies = match effect {
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
        };
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

/// Replacement endpoints and their scheduling proof are compiled together.
/// Execution cannot retain a raw Create while using its prepared target's proof.
///
/// ```compile_fail,E0603
/// use carina_core::replacement::AdmittedReplacementEffects;
/// let unchecked = AdmittedReplacementEffects { effects: Vec::new() };
/// ```
#[derive(Debug)]
pub(crate) struct AdmittedReplacementEffects<S, P: PlanValueKind> {
    effects: Vec<Effect<S, P>>,
    scheduling: ReplacementScheduling,
    temporary_names: PreparedReplacementNames,
}
pub(crate) type PreparedReplacementNames = Vec<(crate::resource::ResourceId, TemporaryName)>;

impl<S: Clone, P: PlanValueKind> AdmittedReplacementEffects<S, P> {
    pub(crate) fn resolve(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
    ) -> Result<Self, CreateBeforeDestroyError> {
        Self::resolve_with_associations(effects, registry, previous, &HashMap::new())
    }
    pub(crate) fn resolve_with_associations(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
        associations: &HashMap<ResolvedResourceId, HashSet<ResolvedResourceId>>,
    ) -> Result<Self, CreateBeforeDestroyError> {
        let mut effects = effects.to_vec();
        let requirements = create_first_requirements(&effects);
        let mut scheduling = ReplacementScheduling::default();
        let mut temporary_names = Vec::new();
        for index in 0..effects.len() {
            if let Effect::DeferredReplace(payload) = &mut effects[index] {
                let required_by = requirements
                    .get(payload.id())
                    .or_else(|| {
                        payload
                            .deletes
                            .iter()
                            .find_map(|old| requirements.get(&old.id))
                    })
                    .cloned();
                if let Some(dependent) = required_by {
                    payload.request = payload.request.clone().require_create_first(dependent);
                }
                payload.preflight(registry)?;
            }
            let Effect::Create(target) = &effects[index] else {
                continue;
            };
            let old: Vec<_> = effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Delete {
                        id,
                        identifier,
                        generation: EffectGeneration::Current,
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
                let mut request = ReplacementRequest::new(target.clone(), old);
                if let Some(dependent) = requirements.get(&target.resolved_id()) {
                    request = request.require_create_first(dependent);
                }
                let checked = resolve_replacement(request, registry)?;
                if let Some(name) = checked.temporary_name() {
                    temporary_names.push((checked.target().id.clone(), name.clone()));
                }
                effects[index] = Effect::Create(checked.target().clone());
                scheduling.insert(checked);
            }
        }
        Ok(Self {
            effects,
            scheduling,
            temporary_names,
        })
    }
    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<Effect<S, P>>,
        ReplacementScheduling,
        PreparedReplacementNames,
    ) {
        (self.effects, self.scheduling, self.temporary_names)
    }
}

/// Scheduling capabilities produced from checked owners. Positions in an
/// effect vector never grant create-first permission.
///
/// ```compile_fail,E0451
/// use carina_core::replacement::ReplacementScheduling;
/// let unchecked = ReplacementScheduling { replacements: Vec::new() };
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplacementScheduling {
    replacements: Vec<CheckedReplacement<ResolvedResourceId>>,
}
impl ReplacementScheduling {
    pub(crate) fn is_create_first(&self, target: &crate::resource::ResourceId) -> bool {
        self.replacements.iter().any(|replacement| {
            replacement.target().as_inner() == target && replacement.is_create_first()
        })
    }
    pub(crate) fn insert<S, P: PlanValueKind>(
        &mut self,
        checked: CheckedReplacement<ResolvedResource<S, P>>,
    ) {
        self.replacements.push(checked.into_scheduling());
    }
    /// Admit all ordinary same-address replacement pairs through the resolver.
    /// This also checks raw endpoints inserted by callers and untrusted plans.
    pub fn resolve<S: Clone, P: PlanValueKind>(
        effects: &[Effect<S, P>],
        registry: &SchemaRegistry,
        previous: impl Fn(&ResolvedResourceId) -> HashMap<String, SnapshotValue>,
    ) -> Result<Self, CreateBeforeDestroyError> {
        Ok(AdmittedReplacementEffects::resolve(effects, registry, previous)?.scheduling)
    }
    pub(crate) fn order_edges<S>(
        &self,
        effects: &[impl AsRef<Effect<S>>],
        deps: &mut HashMap<usize, HashSet<usize>>,
    ) -> HashSet<(usize, usize)> {
        let mut admitted_pairs = HashSet::new();
        for replacement in &self.replacements {
            let Some(create) = effects.iter().position(|effect| matches!(effect.as_ref(), Effect::Create(target) if target.id == **replacement.target())) else { continue; };
            for old in replacement.old_instances() {
                let Some(delete) = effects.iter().position(|effect| matches!(effect.as_ref(), Effect::Delete { id, generation: EffectGeneration::Current, .. } if id == &old.id)) else { continue; };
                admitted_pairs.insert((create, delete));
                if replacement.is_create_first() {
                    deps.entry(delete).or_default().insert(create);
                } else {
                    deps.entry(create).or_default().insert(delete);
                }
            }
        }
        // A checked create-first owner retains its old generation until every
        // dependent operation has switched to its new generation. The child's
        // own replacement order remains independent of this parent window.
        for parent in &self.replacements {
            if !parent.is_create_first() {
                continue;
            }
            let Some(parent_create) = effects.iter().position(|effect| matches!(effect.as_ref(), Effect::Create(resource) if resource.resolved_id() == *parent.target())) else { continue; };
            for old in parent.old_instances() {
                let Some(delete) = effects.iter().position(|effect| matches!(effect.as_ref(), Effect::Delete { id, generation: EffectGeneration::Current, .. } if id == &old.id)) else { continue; };
                for (dependent, effect) in effects.iter().enumerate() {
                    if effect.as_ref().resource_id() != parent.target().as_inner()
                        && matches!(
                            effect.as_ref(),
                            Effect::Create(_)
                                | Effect::ResumeCreate { .. }
                                | Effect::Update { .. }
                                | Effect::Read { .. }
                        )
                        && effect.as_ref().blocking_keys().contains(parent.target())
                    {
                        deps.entry(delete).or_default().insert(dependent);
                        if let Some(child) = self.replacements.iter().find(|child| {
                            child.target().as_inner() == effect.as_ref().resource_id()
                                && !child.is_create_first()
                        }) {
                            for old_child in child.old_instances() {
                                if let Some(child_delete) = effects.iter().position(|candidate| matches!(candidate.as_ref(), Effect::Delete { id, generation: EffectGeneration::Current, .. } if id == &old_child.id)) {
                                    deps.entry(child_delete).or_default().insert(parent_create);
                                }
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
            let mut scheduling = ReplacementScheduling::default();
            scheduling.insert(checked.clone());
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
                let analysis = build_effect_dependency_analysis(
                    &effects,
                    std::iter::empty(),
                    ScheduleInputs::Apply(&scheduling),
                )
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
                ScheduleInputs::Apply(&ReplacementScheduling::default()),
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
            loaded.replacement_scheduling().is_create_first(&target),
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
        let mut scheduling = ReplacementScheduling::default();
        scheduling.insert(parent.clone());
        scheduling.insert(child.clone());
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
        let graph = build_effect_dependency_analysis(
            &effects,
            std::iter::empty(),
            ScheduleInputs::Apply(&scheduling),
        )
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
}
