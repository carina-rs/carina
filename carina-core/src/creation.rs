//! Creation admission belongs to finalized effects, after replacement and
//! state-block processing. These witnesses are deliberately never persisted.

use std::{
    collections::{BTreeMap, HashMap},
    ops::Deref,
    sync::Arc,
};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{
    effect::{Effect, EffectGeneration, EffectInstanceKey},
    plan::{Plan, PlanError, PlanErrorKind},
    resource::{ConcreteValue, Current, DeferredValue, ModuleSource, ResourceId, Value, ValueKind},
    schema::{
        Compiled, CreationInputPath, CreationRule, CreationRuleCode, CreationViolation, InputMode,
        SchemaKind, SchemaRegistry, ValidatedResourceSchema,
    },
};

/// The operation that will send an initial create request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreationContext {
    Create,
    Replacement { create_before_destroy: bool },
    ResumeCreate,
    DeferredCreate,
    DeferredReplacement,
}

#[derive(Debug, Clone)]
struct CreationPlanIdentity(Arc<()>);

impl PartialEq for CreationPlanIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CreationPlanIdentity {}

/// A slot in one immutable finalized plan. The index distinguishes effects
/// even when several effects address the same resource/generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationEffectTarget {
    plan: CreationPlanIdentity,
    effect_index: usize,
    instance: EffectInstanceKey,
    template: bool,
}

impl CreationEffectTarget {
    pub fn effect_index(&self) -> usize {
        self.effect_index
    }
    pub fn instance(&self) -> &EffectInstanceKey {
        &self.instance
    }
    pub fn is_template(&self) -> bool {
        self.template
    }
}

/// A delete-first replacement's exact delete slot, including absorbed deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedCreationDelete {
    plan: CreationPlanIdentity,
    effect_index: usize,
    absorbed_index: Option<usize>,
    instance: EffectInstanceKey,
}

impl PairedCreationDelete {
    pub fn effect_index(&self) -> usize {
        self.effect_index
    }
    pub fn absorbed_index(&self) -> Option<usize> {
        self.absorbed_index
    }
    pub fn instance(&self) -> &EffectInstanceKey {
        &self.instance
    }
}

/// Only the consuming finalized-plan transition can construct this view.
/// Raw resources, raw effects and normalization inputs carry no such authority.
///
/// ```compile_fail
/// use carina_core::{creation::CreateEffectView, resource::Resource};
/// let view: CreateEffectView<'_> = Resource::new("test", "r").into();
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreateEffectView, effect::Effect};
/// fn forge(effect: &Effect) -> CreateEffectView<'_> { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreateEffectView, effect::Effect};
/// fn forge(effect: Effect) -> CreateEffectView<'static> { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::creation::CreateEffectView;
/// let view: CreateEffectView<'_> = Default::default();
/// ```
/// ```compile_fail
/// use carina_core::creation::CreateEffectView;
/// let view: CreateEffectView<'_> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::creation::CreateEffectView;
/// fn mutate(view: &mut CreateEffectView<'_>) { view.attributes = None; }
/// ```
pub struct CreateEffectView<'a> {
    target: CreationEffectTarget,
    resource_id: &'a ResourceId,
    attributes: Option<&'a IndexMap<String, Value>>,
    context: CreationContext,
    paired_deletes: Vec<PairedCreationDelete>,
}

impl CreateEffectView<'_> {
    pub fn target(&self) -> &CreationEffectTarget {
        &self.target
    }
    pub fn resource_id(&self) -> &ResourceId {
        self.resource_id
    }
    pub fn context(&self) -> CreationContext {
        self.context
    }
}

/// An explicit remainder of admission; it cannot be deserialized or discharged
/// by callers. Deferred slots bind to the template effect, never a name.
///
/// ```compile_fail
/// use carina_core::creation::PendingCreationObligation;
/// let pending: PendingCreationObligation = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::creation::PendingCreationObligation;
/// fn persist(pending: &PendingCreationObligation) { let _ = serde_json::to_string(pending); }
/// ```
#[derive(Debug, Clone)]
pub struct PendingCreationObligation {
    target: CreationEffectTarget,
    rules: Vec<CreationRule<Compiled>>,
    paired_deletes: Vec<PairedCreationDelete>,
}

impl PendingCreationObligation {
    pub fn target(&self) -> &CreationEffectTarget {
        &self.target
    }
    pub fn rules(&self) -> &[CreationRule<Compiled>] {
        &self.rules
    }
    pub fn paired_delete(&self) -> Option<&PairedCreationDelete> {
        self.paired_deletes.first()
    }
    pub fn paired_deletes(&self) -> &[PairedCreationDelete] {
        &self.paired_deletes
    }
}

/// Finalization has completed all effect rewrites. There is no public raw-plan
/// constructor or mutable access; creation admission consumes this owner.
///
/// ```compile_fail
/// use carina_core::{creation::FinalizedPlan, plan::Plan};
/// let finalized: FinalizedPlan = Plan::new().into();
/// ```
/// ```compile_fail
/// use carina_core::{plan::Plan, schema::SchemaRegistry};
/// fn bypass(raw: Plan, schemas: &SchemaRegistry) {
///     let _ = raw.check_creation(schemas, &std::collections::HashMap::new());
/// }
/// ```
#[derive(Debug)]
pub struct FinalizedPlan<S = ModuleSource> {
    plan: Plan<S>,
    identity: CreationPlanIdentity,
}

/// Rejected admission retains the complete diagnostic plan, without creating
/// display, persistence, or execution authority.
pub struct CreationAdmissionFailure<S = ModuleSource> {
    plan: Box<Plan<S>>,
}

impl<S> std::fmt::Debug for CreationAdmissionFailure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreationAdmissionFailure")
            .field("errors", &self.plan.errors())
            .finish()
    }
}

impl<S> Deref for CreationAdmissionFailure<S> {
    type Target = Plan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> CreationAdmissionFailure<S> {
    pub fn into_plan(self) -> Plan<S> {
        *self.plan
    }
}

impl<S> Deref for FinalizedPlan<S> {
    type Target = Plan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> FinalizedPlan<S> {
    pub(crate) fn new(plan: Plan<S>) -> Self {
        Self {
            plan,
            identity: CreationPlanIdentity(Arc::new(())),
        }
    }

    /// Admit every final create half and resend-capable continuation. Existing
    /// diagnostics are retained; new diagnostics are sorted by routed address,
    /// path and code independently of declaration order.
    pub fn check_creation(
        mut self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<CreationCheckedPlan<S>, CreationAdmissionFailure<S>> {
        let replacements = self
            .plan
            .replace_display_info()
            .map(|info| (info.create_idx, info))
            .collect::<HashMap<_, _>>();
        let mut diagnostics = Vec::new();
        let mut pending = Vec::new();
        for (index, effect) in self.plan.effects().iter().enumerate() {
            let (resource_id, attributes, context, template, paired_deletes) = match effect {
                Effect::Create(resource) => {
                    let replacement = replacements.get(&index);
                    let deletes = replacement
                        .filter(|info| !info.create_before_destroy)
                        .map(|info| {
                            let delete = &self.plan.effects()[info.delete_idx];
                            vec![PairedCreationDelete {
                                plan: self.identity.clone(),
                                effect_index: info.delete_idx,
                                absorbed_index: None,
                                instance: delete.instance_key(),
                            }]
                        })
                        .unwrap_or_default();
                    (
                        &resource.id,
                        Some(&resource.attributes),
                        replacement.map_or(CreationContext::Create, |info| {
                            CreationContext::Replacement {
                                create_before_destroy: info.create_before_destroy,
                            }
                        }),
                        false,
                        deletes,
                    )
                }
                Effect::ResumeCreate { resource, .. } => {
                    if matches!(current.get(&resource.id), Some(Current::CreateInFlight(pending)) if pending.identifier().is_some())
                    {
                        continue;
                    }
                    (
                        &resource.id,
                        Some(&resource.attributes),
                        CreationContext::ResumeCreate,
                        false,
                        Vec::new(),
                    )
                }
                Effect::DeferredCreate(payload) => (
                    payload.template.template_resource_id(),
                    None,
                    CreationContext::DeferredCreate,
                    true,
                    Vec::new(),
                ),
                Effect::DeferredReplace(payload) => {
                    let deletes = payload
                        .deletes
                        .iter()
                        .enumerate()
                        .map(|(absorbed, delete)| PairedCreationDelete {
                            plan: self.identity.clone(),
                            effect_index: index,
                            absorbed_index: Some(absorbed),
                            instance: EffectInstanceKey::new(
                                delete.id.clone(),
                                EffectGeneration::Current,
                            ),
                        })
                        .collect();
                    (
                        payload.template.template_resource_id(),
                        None,
                        CreationContext::DeferredReplacement,
                        true,
                        deletes,
                    )
                }
                Effect::Read { .. }
                | Effect::Update { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. } => continue,
            };
            let Some(schema) = schemas.get(
                &resource_id.provider,
                &resource_id.resource_type,
                SchemaKind::Resource,
            ) else {
                continue;
            };
            let view = CreateEffectView {
                target: CreationEffectTarget {
                    plan: self.identity.clone(),
                    effect_index: index,
                    instance: effect.instance_key(),
                    template,
                },
                resource_id,
                attributes,
                context,
                paired_deletes,
            };
            let mut undecided = Vec::new();
            let mut rules = schema
                .schema()
                .as_schema()
                .creation_constraints()
                .iter()
                .collect::<Vec<_>>();
            rules.sort_by(|a, b| (a.path(), a.code()).cmp(&(b.path(), b.code())));
            for rule in rules {
                match verdict(&view, rule, schema.schema()) {
                    CreationVerdict::Proven => {}
                    CreationVerdict::Violation(violation) => diagnostics.push(PlanError::new(
                        resource_id.clone(),
                        PlanErrorKind::CreationConstraint {
                            path: violation.path().as_str().into(),
                            code: violation.code().as_str().into(),
                            reason: violation.reason().to_string(),
                            context,
                        },
                    )),
                    CreationVerdict::Undecided => undecided.push(rule.clone()),
                }
            }
            if !undecided.is_empty() {
                pending.push(PendingCreationObligation {
                    target: view.target,
                    rules: undecided,
                    paired_deletes: view.paired_deletes,
                });
            }
        }
        diagnostics.sort_by(|a, b| match (&a.kind, &b.kind) {
            (
                PlanErrorKind::CreationConstraint {
                    path: path_a,
                    code: code_a,
                    ..
                },
                PlanErrorKind::CreationConstraint {
                    path: path_b,
                    code: code_b,
                    ..
                },
            ) => {
                // Display spelling omits provider-instance routing. Order by
                // the complete routed identity before the path and rule code.
                let a = &a.resource_id;
                let b = &b.resource_id;
                (
                    &a.provider,
                    &a.provider_instance,
                    &a.resource_type,
                    a.identity_str(),
                    path_a,
                    code_a,
                )
                    .cmp(&(
                        &b.provider,
                        &b.provider_instance,
                        &b.resource_type,
                        b.identity_str(),
                        path_b,
                        code_b,
                    ))
            }
            _ => unreachable!("only creation diagnostics are collected here"),
        });
        for error in diagnostics {
            self.plan.add_error(error);
        }
        if self.plan.has_errors() {
            Err(CreationAdmissionFailure {
                plan: Box::new(self.plan),
            })
        } else {
            Ok(CreationCheckedPlan {
                plan: self.plan,
                pending,
            })
        }
    }
}

enum CreationVerdict {
    Proven,
    Violation(CreationViolation),
    Undecided,
}

fn verdict(
    view: &CreateEffectView<'_>,
    rule: &CreationRule<Compiled>,
    schema: &ValidatedResourceSchema,
) -> CreationVerdict {
    let Some(attributes) = view.attributes else {
        return estimate_unknown(view, rule);
    };
    let path = rule
        .path()
        .segments()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut fields = attributes;
    for (index, segment) in path.iter().enumerate() {
        let Some(value) = fields.get(segment) else {
            return if schema
                .effective_node_at_path(&path[..=index])
                .is_some_and(|node| node.effective_input_mode() == InputMode::Optional)
            {
                CreationVerdict::Proven
            } else {
                estimate_unknown(view, rule)
            };
        };
        let value = peel_secret(value);
        if index + 1 == path.len() {
            return match value {
                Value::Concrete(ConcreteValue::String(value)) => rule
                    .evaluate(value.expose_plaintext())
                    .map_or_else(CreationVerdict::Violation, |()| CreationVerdict::Proven),
                Value::Concrete(ConcreteValue::CanonicalEnum(value)) => rule
                    .evaluate(value.api_value())
                    .map_or_else(CreationVerdict::Violation, |()| CreationVerdict::Proven),
                _ => estimate_unknown(view, rule),
            };
        }
        fields = match value {
            Value::Concrete(ConcreteValue::Map(fields)) => fields,
            _ => return estimate_unknown(view, rule),
        };
    }
    unreachable!("admitted creation paths are nonempty")
}

fn peel_secret<P: ValueKind>(mut value: &Value<P>) -> &Value<P> {
    while let Value::Deferred(DeferredValue::Secret(secret)) = value {
        value = secret.expose_plaintext();
    }
    value
}

/// Phase 3 replaces this one seam with estimation from authored expressions and
/// their immutable type context. Erased unknowns never manufacture proof.
fn estimate_unknown(
    _view: &CreateEffectView<'_>,
    _rule: &CreationRule<Compiled>,
) -> CreationVerdict {
    CreationVerdict::Undecided
}

/// Every creating slot either passed or retains an explicit obligation.
/// Cloning this immutable owner preserves the same effects; mutable/raw plans
/// cannot recover its execution authority.
///
/// ```compile_fail
/// use carina_core::{creation::CreationCheckedPlan, plan::Plan};
/// let checked: CreationCheckedPlan = Plan::new().into();
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreationCheckedPlan, resource::Resource};
/// let checked: CreationCheckedPlan = Resource::new("test", "r").into();
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreationCheckedPlan, effect::Effect};
/// fn forge(effect: Effect) -> CreationCheckedPlan { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreationCheckedPlan, plan::Plan};
/// let checked = CreationCheckedPlan { plan: Plan::new(), pending: Vec::new() };
/// ```
/// ```compile_fail
/// use carina_core::creation::CreationCheckedPlan;
/// let checked: CreationCheckedPlan = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::{creation::CreationCheckedPlan, effect::Effect};
/// fn mutate(checked: &mut CreationCheckedPlan, effect: Effect) { checked.add(effect); }
/// ```
/// ```compile_fail
/// use carina_core::creation::CreationCheckedPlan;
/// let checked = CreationCheckedPlan::default();
/// ```
#[derive(Debug, Clone)]
pub struct CreationCheckedPlan<S = ModuleSource> {
    plan: Plan<S>,
    pending: Vec<PendingCreationObligation>,
}

impl<S> Deref for CreationCheckedPlan<S> {
    type Target = Plan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> CreationCheckedPlan<S> {
    pub fn plan(&self) -> &Plan<S> {
        &self.plan
    }
    pub fn pending_creation(&self) -> &[PendingCreationObligation] {
        &self.pending
    }
    pub fn ensure_creation_ready(&self) -> Result<(), CreationCheckPending> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(CreationCheckPending)
        }
    }
    pub(crate) fn pending_rows(&self, effect_index: usize) -> Vec<crate::detail_rows::DetailRow> {
        let mut paths: BTreeMap<CreationInputPath, Vec<CreationRuleCode>> = BTreeMap::new();
        for obligation in &self.pending {
            if obligation.target.effect_index == effect_index {
                for rule in &obligation.rules {
                    paths
                        .entry(rule.path().clone())
                        .or_default()
                        .push(rule.code().clone());
                }
            }
        }
        paths
            .into_iter()
            .map(|(path, mut codes)| {
                codes.sort();
                codes.dedup();
                crate::detail_rows::DetailRow::PendingCreation { path, codes }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("creation check pending; apply-time discharge not implemented")]
pub struct CreationCheckPending;
