//! Scoped desired-input admission after effect finalization. Only this module
//! classifies write sites and evaluates rules. Witnesses are never persisted.

use std::{
    collections::{BTreeMap, HashMap},
    ops::Deref,
    sync::Arc,
};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

mod estimator;
mod type_context;
pub use type_context::InputTypeContext;
use type_context::TypeFacts;

#[cfg(test)]
mod estimator_tests;

use crate::{
    effect::{Effect, EffectGeneration, EffectInstanceKey},
    plan::{Plan, PlanError, PlanErrorKind},
    resource::{ConcreteValue, Current, DeferredValue, ModuleSource, ResourceId, Value, ValueKind},
    schema::{
        Compiled, InputMode, InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope,
        InputRuleTarget, InputViolation, InputViolationReason, SchemaKind, SchemaRegistry,
        ValidatedResourceSchema,
    },
};

/// The desired-write context selected by the sole site classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputWriteContext {
    Create,
    Update,
    AuthoredDesired,
    Replacement { create_before_destroy: bool },
    ResumeCreate,
    DeferredCreate,
    DeferredReplacement,
}

#[derive(Debug, Clone)]
struct InputPlanIdentity(Arc<()>);

impl PartialEq for InputPlanIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for InputPlanIdentity {}

/// A slot in one immutable finalized plan. The index distinguishes effects
/// even when several effects address the same resource/generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEffectTarget {
    plan: InputPlanIdentity,
    effect_index: usize,
    instance: EffectInstanceKey,
    template: bool,
}

impl InputEffectTarget {
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
pub struct PairedInputDelete {
    plan: InputPlanIdentity,
    effect_index: usize,
    absorbed_index: Option<usize>,
    instance: EffectInstanceKey,
}

impl PairedInputDelete {
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

/// Only the write-site classifier can construct this view.
/// Raw resources, raw effects and normalization inputs carry no such authority.
///
/// ```compile_fail
/// use carina_core::{input_rules::WriteSite, resource::Resource};
/// let view: WriteSite<'_> = Resource::new("test", "r").into();
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::WriteSite, effect::Effect};
/// fn forge(effect: &Effect) -> WriteSite<'_> { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::WriteSite, effect::Effect};
/// fn forge(effect: Effect) -> WriteSite<'static> { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::input_rules::WriteSite;
/// let view: WriteSite<'_> = Default::default();
/// ```
/// ```compile_fail
/// use carina_core::input_rules::WriteSite;
/// let view: WriteSite<'_> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::input_rules::WriteSite;
/// fn mutate(view: &mut WriteSite<'_>) { view.attributes = None; }
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::WriteSite, resource::Current};
/// fn forge(current: &Current) -> WriteSite<'_> { current.into() }
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::WriteSite, resource::State};
/// fn forge(state: &State) -> WriteSite<'_> { state.into() }
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::ApplicableInputRule, schema::{Compiled, InputRule}};
/// fn bypass(rule: &InputRule<Compiled>) {
///     let applicable = ApplicableInputRule { rule };
///     let _ = applicable.evaluate();
/// }
/// ```
/// The executor constructor requires discharge authority and is inaccessible
/// outside core.
///
/// ```compile_fail
/// use carina_core::input_rules::executor_write_site;
/// use carina_core::executor::input_discharge::DischargeToken;
/// ```
pub struct WriteSite<'a> {
    target: Option<InputEffectTarget>,
    resource_id: &'a ResourceId,
    attributes: Option<&'a IndexMap<String, Value>>,
    authored: Option<AuthoredAttributes<'a>>,
    types: Option<&'a TypeFacts>,
    context: InputWriteContext,
    schema: &'a ValidatedResourceSchema,
    rules: Vec<&'a InputRule<Compiled>>,
    paired_deletes: Vec<PairedInputDelete>,
}

enum AuthoredAttributes<'a> {
    Resource(&'a IndexMap<String, Value>),
    Template(&'a IndexMap<String, Value<crate::resource::LoopSlot>>),
}

impl WriteSite<'_> {
    pub fn target(&self) -> Option<&InputEffectTarget> {
        self.target.as_ref()
    }
    pub fn applicable_rules(&self) -> impl Iterator<Item = ApplicableInputRule<'_, '_>> {
        self.rules
            .iter()
            .map(|rule| ApplicableInputRule { site: self, rule })
    }
    pub fn resource_id(&self) -> &ResourceId {
        self.resource_id
    }
    pub fn context(&self) -> InputWriteContext {
        self.context
    }
}

/// An applicable declaration bound to the desired projection selected by the
/// classifier. Callers supply neither a rule nor a value when evaluating it.
pub struct ApplicableInputRule<'s, 'a> {
    site: &'s WriteSite<'a>,
    rule: &'a InputRule<Compiled>,
}

impl ApplicableInputRule<'_, '_> {
    pub(crate) fn path(&self) -> &InputPath {
        self.rule.path()
    }
    pub(crate) fn code(&self) -> &InputRuleCode {
        self.rule.code()
    }
    pub fn evaluate(&self) -> InputVerdict {
        verdict(self.site, self.rule)
    }
}

enum WriteSiteSource<'a, S> {
    Finalized {
        plan: &'a FinalizedPlan<S>,
        index: usize,
        current: &'a HashMap<ResourceId, Current>,
        types: Option<&'a TypeFacts>,
    },
    Authored(&'a crate::resource::Resource),
    Executor {
        authority: &'a crate::executor::input_discharge::DischargeToken,
        admission: &'a PendingInputObligation,
        resource_id: &'a ResourceId,
        attributes: &'a IndexMap<String, Value>,
    },
}

/// The sole classifier and constructor of desired write sites. Current is
/// consulted only for the issued/unissued continuation distinction; its values
/// never enter the checked projection. Updates always select `to`, never `from`.
fn classify_write_site<'a, S>(
    source: WriteSiteSource<'a, S>,
    schemas: &'a SchemaRegistry,
) -> Option<WriteSite<'a>> {
    let executor_admission = match &source {
        WriteSiteSource::Executor { admission, .. } => Some(*admission),
        _ => None,
    };
    let (types, authored) = match &source {
        WriteSiteSource::Executor { .. } => (None, None),
        WriteSiteSource::Authored(resource) => (
            None,
            Some(AuthoredAttributes::Resource(&resource.attributes)),
        ),
        WriteSiteSource::Finalized {
            plan, index, types, ..
        } => {
            let effect = &plan.plan.effects()[*index];
            let authored = match effect {
                Effect::DeferredCreate(payload) => Some(AuthoredAttributes::Template(
                    payload.template.template_resource().attributes(),
                )),
                Effect::DeferredReplace(payload) => Some(AuthoredAttributes::Template(
                    payload.template.template_resource().attributes(),
                )),
                _ => types
                    .and_then(|types| types.authored.get(effect.resource_id()))
                    .map(AuthoredAttributes::Resource),
            };
            (*types, authored)
        }
    };
    let (target, resource_id, attributes, context, paired_deletes) = match source {
        WriteSiteSource::Executor {
            authority,
            admission,
            resource_id,
            attributes,
        } => {
            let _ = authority;
            (
                Some(admission.target.clone()),
                resource_id,
                Some(attributes),
                admission.context,
                admission.paired_deletes.clone(),
            )
        }
        WriteSiteSource::Authored(resource) => (
            None,
            &resource.id,
            Some(&resource.attributes),
            InputWriteContext::AuthoredDesired,
            Vec::new(),
        ),
        WriteSiteSource::Finalized {
            plan,
            index,
            current,
            types: _,
        } => {
            let effect = &plan.plan.effects()[index];
            let (resource_id, attributes, context, template, deletes) = match effect {
                Effect::Create(resource) => {
                    let replacement = plan.replacement_creates.get(&index);
                    let deletes = replacement
                        .as_ref()
                        .filter(|info| !info.create_before_destroy)
                        .map(|info| {
                            vec![PairedInputDelete {
                                plan: plan.identity.clone(),
                                effect_index: info.delete_index,
                                absorbed_index: None,
                                instance: plan.plan.effects()[info.delete_index].instance_key(),
                            }]
                        })
                        .unwrap_or_default();
                    (
                        &resource.id,
                        Some(&resource.attributes),
                        replacement.map_or(InputWriteContext::Create, |info| {
                            InputWriteContext::Replacement {
                                create_before_destroy: info.create_before_destroy,
                            }
                        }),
                        false,
                        deletes,
                    )
                }
                Effect::Update {
                    from: _,
                    identifier: _,
                    to,
                    changed_attributes: _,
                } => (
                    &to.id,
                    Some(&to.attributes),
                    InputWriteContext::Update,
                    false,
                    Vec::new(),
                ),
                Effect::ResumeCreate {
                    resource,
                    started_at: _,
                } => {
                    if matches!(current.get(&resource.id), Some(Current::CreateInFlight(pending)) if pending.identifier().is_some())
                    {
                        return None;
                    }
                    (
                        &resource.id,
                        Some(&resource.attributes),
                        InputWriteContext::ResumeCreate,
                        false,
                        Vec::new(),
                    )
                }
                Effect::DeferredCreate(payload) => (
                    payload.template.template_resource_id(),
                    None,
                    InputWriteContext::DeferredCreate,
                    true,
                    Vec::new(),
                ),
                Effect::DeferredReplace(payload) => {
                    let deletes = payload
                        .deletes
                        .iter()
                        .enumerate()
                        .map(|(absorbed, delete)| PairedInputDelete {
                            plan: plan.identity.clone(),
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
                        InputWriteContext::DeferredReplacement,
                        true,
                        deletes,
                    )
                }
                Effect::Read { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. } => return None,
            };
            (
                Some(InputEffectTarget {
                    plan: plan.identity.clone(),
                    effect_index: index,
                    instance: effect.instance_key(),
                    template,
                }),
                resource_id,
                attributes,
                context,
                deletes,
            )
        }
    };
    let schema = match executor_admission {
        Some(admission) => admission.schema.as_ref(),
        None => schemas
            .get(
                &resource_id.provider,
                &resource_id.resource_type,
                SchemaKind::Resource,
            )?
            .schema(),
    };
    // Discharge must consume the declarations that were admitted. A changed
    // registry cannot erase an obligation or change its optional-path meaning.
    let declarations = executor_admission.map_or_else(
        || schema.as_schema().input_rules(),
        |admission| admission.rules.as_slice(),
    );
    let mut rules = declarations
        .iter()
        .filter(|rule| {
            // Exhaustive in both scope and write context: adding either requires a
            // deliberate applicability decision here, before evaluation is possible.
            match rule.scope() {
                InputRuleScope::CreateOnly => match context {
                    InputWriteContext::Create
                    | InputWriteContext::Replacement { .. }
                    | InputWriteContext::ResumeCreate
                    | InputWriteContext::DeferredCreate
                    | InputWriteContext::DeferredReplacement => true,
                    InputWriteContext::Update | InputWriteContext::AuthoredDesired => false,
                },
                InputRuleScope::EveryWrite => match context {
                    InputWriteContext::Create
                    | InputWriteContext::Replacement { .. }
                    | InputWriteContext::ResumeCreate
                    | InputWriteContext::DeferredCreate
                    | InputWriteContext::DeferredReplacement
                    | InputWriteContext::Update
                    | InputWriteContext::AuthoredDesired => true,
                },
            }
        })
        .collect::<Vec<_>>();
    rules.sort_by(|a, b| (a.target(), a.code()).cmp(&(b.target(), b.code())));
    Some(WriteSite {
        target,
        resource_id,
        attributes,
        authored,
        types,
        context,
        schema,
        rules,
        paired_deletes,
    })
}

/// An explicit remainder of admission; it cannot be deserialized or discharged
/// by callers. Deferred slots bind to the template effect, never a name.
///
/// ```compile_fail
/// use carina_core::input_rules::PendingInputObligation;
/// let pending: PendingInputObligation = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::input_rules::PendingInputObligation;
/// fn persist(pending: &PendingInputObligation) { let _ = serde_json::to_string(pending); }
/// ```
#[derive(Debug, Clone)]
pub struct PendingInputObligation {
    target: InputEffectTarget,
    rules: Vec<InputRule<Compiled>>,
    paired_deletes: Vec<PairedInputDelete>,
    context: InputWriteContext,
    schema: Arc<ValidatedResourceSchema>,
}

impl PendingInputObligation {
    pub fn target(&self) -> &InputEffectTarget {
        &self.target
    }
    pub fn rules(&self) -> &[InputRule<Compiled>] {
        &self.rules
    }
    pub fn paired_delete(&self) -> Option<&PairedInputDelete> {
        self.paired_deletes.first()
    }
    pub fn paired_deletes(&self) -> &[PairedInputDelete] {
        &self.paired_deletes
    }
}

/// Finalization has completed all effect rewrites. There is no public raw-plan
/// constructor or mutable access; input admission consumes this owner.
///
/// ```compile_fail
/// use carina_core::{input_rules::FinalizedPlan, plan::Plan};
/// let finalized: FinalizedPlan = Plan::new().into();
/// ```
/// ```compile_fail
/// use carina_core::{plan::Plan, schema::SchemaRegistry};
/// fn bypass(raw: Plan, schemas: &SchemaRegistry) {
///     let _ = raw.check_inputs(schemas, &std::collections::HashMap::new());
/// }
/// ```
#[derive(Debug)]
pub struct FinalizedPlan<S = ModuleSource> {
    plan: Plan<S>,
    identity: InputPlanIdentity,
    replacement_creates: HashMap<usize, ReplacementWrite>,
}

#[derive(Debug)]
struct ReplacementWrite {
    delete_index: usize,
    create_before_destroy: bool,
}

/// Rejected admission retains the complete diagnostic plan, without creating
/// display, persistence, or execution authority.
pub struct InputAdmissionFailure<S = ModuleSource> {
    plan: Box<Plan<S>>,
}

impl<S> std::fmt::Debug for InputAdmissionFailure<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputAdmissionFailure")
            .field("errors", &self.plan.errors())
            .finish()
    }
}

impl<S> Deref for InputAdmissionFailure<S> {
    type Target = Plan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> InputAdmissionFailure<S> {
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

impl FinalizedPlan<crate::resource::SavedModuleSource> {
    /// Seal the saved effects after the caller has verified and rehydrated
    /// them. This performs no diff or effect rewrite and grants no execution
    /// authority until the ordinary input-admission transition succeeds.
    pub fn from_saved_plan(plan: Plan<crate::resource::SavedModuleSource>) -> Self {
        Self::new(plan)
    }
}

impl<S> FinalizedPlan<S> {
    pub(crate) fn new(plan: Plan<S>) -> Self {
        let replacement_creates = plan
            .replace_display_info()
            .map(|info| {
                (
                    info.create_idx,
                    ReplacementWrite {
                        delete_index: info.delete_idx,
                        create_before_destroy: info.create_before_destroy,
                    },
                )
            })
            .collect();
        Self {
            plan,
            identity: InputPlanIdentity(Arc::new(())),
            replacement_creates,
        }
    }

    /// Borrow classified desired sites from this sealed plan. Rules can be
    /// evaluated only against the projection retained by each returned site.
    pub fn write_sites<'a>(
        &'a self,
        schemas: &'a SchemaRegistry,
        current: &'a HashMap<ResourceId, Current>,
    ) -> impl Iterator<Item = WriteSite<'a>> {
        (0..self.plan.effects().len()).filter_map(move |index| {
            classify_write_site(
                WriteSiteSource::Finalized {
                    plan: self,
                    index,
                    current,
                    types: None,
                },
                schemas,
            )
        })
    }

    /// Admit every final desired write and resend-capable continuation. Existing
    /// diagnostics are retained; new diagnostics are sorted by routed address,
    /// path and code independently of declaration order.
    pub fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>> {
        self.check_inputs_with_types(schemas, current, &InputTypeContext::empty())
    }

    /// Admit using the one immutable declaration context retained for this plan.
    pub fn check_inputs_with_types(
        mut self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
        types: &InputTypeContext<S>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>> {
        let mut diagnostics = Vec::new();
        let mut pending = Vec::new();
        let mut write_checks = Vec::new();
        for index in 0..self.plan.effects().len() {
            let Some(view) = classify_write_site(
                WriteSiteSource::Finalized {
                    plan: &self,
                    index,
                    current,
                    types: Some(&types.facts),
                },
                schemas,
            ) else {
                continue;
            };
            let resource_id = view.resource_id;
            let context = view.context;
            let mut undecided = Vec::new();
            // Normalization can change even statically proven values. Retain
            // the classified authority for rechecking the exact wire request.
            if !view.rules.is_empty() {
                write_checks.push(PendingInputObligation {
                    target: view.target.clone().expect("finalized slot"),
                    rules: view.rules.iter().map(|rule| (*rule).clone()).collect(),
                    paired_deletes: view.paired_deletes.clone(),
                    context,
                    schema: Arc::new(view.schema.clone()),
                });
            }
            for applicable in view.applicable_rules() {
                let rule = applicable.rule;
                match applicable.evaluate() {
                    InputVerdict::Proven => {}
                    InputVerdict::Violation(violation) => diagnostics.push(PlanError::new(
                        resource_id.clone(),
                        PlanErrorKind::InputRule {
                            path: violation.path().as_str().into(),
                            code: violation.code().as_str().into(),
                            reason: violation.reason().to_string(),
                            context,
                        },
                    )),
                    InputVerdict::Undecided => undecided.push(rule.clone()),
                }
            }
            if !undecided.is_empty() {
                pending.push(PendingInputObligation {
                    target: view
                        .target
                        .expect("finalized sites carry an exact effect slot"),
                    rules: undecided,
                    paired_deletes: view.paired_deletes,
                    context,
                    schema: Arc::new(view.schema.clone()),
                });
            }
        }
        diagnostics.sort_by(|a, b| match (&a.kind, &b.kind) {
            (
                PlanErrorKind::InputRule {
                    path: path_a,
                    code: code_a,
                    ..
                },
                PlanErrorKind::InputRule {
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
            _ => unreachable!("only input-rule diagnostics are collected here"),
        });
        for error in diagnostics {
            self.plan.add_error(error);
        }
        if self.plan.has_errors() {
            Err(InputAdmissionFailure {
                plan: Box::new(self.plan),
            })
        } else {
            Ok(InputCheckedPlan {
                plan: self.plan,
                obligations: Box::new(InputObligations {
                    pending,
                    write_checks,
                }),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputVerdict {
    Proven,
    Violation(InputViolation),
    Undecided,
}

fn verdict(view: &WriteSite<'_>, rule: &InputRule<Compiled>) -> InputVerdict {
    let mut verdict = projected_verdict(view, rule);
    if matches!(verdict, InputVerdict::Violation(_)) {
        return verdict;
    }
    if matches!(rule.target(), InputRuleTarget::MapKeys(path) if path.as_str() == "tags")
        && let Some(keys) = view
            .types
            .and_then(|types| types.default_keys.get(view.resource_id))
    {
        for (key, mandatory) in keys {
            if let Err(violation) = evaluate_string(rule, key) {
                if *mandatory {
                    return InputVerdict::Violation(violation);
                }
                verdict = InputVerdict::Undecided;
            }
        }
    }
    verdict
}

fn projected_verdict(view: &WriteSite<'_>, rule: &InputRule<Compiled>) -> InputVerdict {
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
            return if view
                .schema
                .effective_node_at_path(&path[..=index])
                .is_some_and(|node| node.effective_input_mode() == InputMode::Optional)
            {
                InputVerdict::Proven
            } else {
                estimate_unknown(view, rule)
            };
        };
        let value = peel_secret(value);
        if index + 1 == path.len() {
            return match rule.target() {
                InputRuleTarget::Value(_) => match value {
                    Value::Concrete(ConcreteValue::String(value)) => {
                        evaluate_string(rule, value.expose_plaintext())
                            .map_or_else(InputVerdict::Violation, |()| InputVerdict::Proven)
                    }
                    Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                        evaluate_string(rule, value.api_value())
                            .map_or_else(InputVerdict::Violation, |()| InputVerdict::Proven)
                    }
                    _ => estimate_unknown(view, rule),
                },
                InputRuleTarget::MapKeys(_) => match value {
                    Value::Concrete(ConcreteValue::Map(entries)) => {
                        for key in entries.keys() {
                            if let Err(violation) = evaluate_string(rule, key) {
                                return InputVerdict::Violation(violation);
                            }
                        }
                        InputVerdict::Proven
                    }
                    _ => estimate_unknown(view, rule),
                },
            };
        }
        fields = match value {
            Value::Concrete(ConcreteValue::Map(fields)) => fields,
            _ => return estimate_unknown(view, rule),
        };
    }
    unreachable!("admitted input paths are nonempty")
}

fn peel_secret<P: ValueKind>(mut value: &Value<P>) -> &Value<P> {
    while let Value::Deferred(DeferredValue::Secret(secret)) = value {
        value = secret.expose_plaintext();
    }
    value
}

/// Estimate only the classifier-selected source expression. Erased unknowns
/// provide no type evidence.
fn estimate_unknown(view: &WriteSite<'_>, rule: &InputRule<Compiled>) -> InputVerdict {
    estimator::estimate(view, rule)
}

/// A value-free diagnostic on authored input, shared by validate and the LSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredInputFinding {
    pub resource_id: ResourceId,
    pub violation: InputViolation,
}

impl std::fmt::Display for AuthoredInputFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.resource_id, self.violation)
    }
}

/// Check known EveryWrite inputs in a parser-owned configuration. Raw resource,
/// state and attribute-map callers cannot manufacture this source authority.
/// CreateOnly declarations are filtered by the same classifier used by plans.
pub fn check_authored_inputs<E, P, T, D, B>(
    parsed: &crate::parser::File<E, P, T, D, B>,
    schemas: &SchemaRegistry,
) -> crate::provider::ProviderResult<Vec<AuthoredInputFinding>>
where
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
{
    use crate::parser::ResourceRef;
    let mut resources = parsed
        .iter_all_resources()
        .filter_map(|source| {
            // Only managed authored resources enter this projection. Read models
            // and data sources have no path into a desired write site.
            let (id, attributes) = match source {
                ResourceRef::Resource(resource) => {
                    (&resource.id, project_attributes(&resource.attributes))
                }
                ResourceRef::Deferred {
                    resource,
                    deferred: _,
                } => (resource.id(), project_attributes(resource.attributes())),
                ResourceRef::DataSource(_) | ResourceRef::LiveComposition(_) => return None,
            };
            let mut resource = crate::resource::Resource::with_provider(
                &id.provider,
                &id.resource_type,
                "_input_projection",
                id.provider_instance.clone(),
            );
            resource.id = id.clone();
            resource.attributes = attributes;
            Some(resource)
        })
        .collect::<Vec<_>>();
    for config in &parsed.providers {
        let crate::parser::ProviderConfig {
            name,
            attributes,
            default_tags,
            source,
            version,
            revision,
            unresolved_attributes,
            binding,
        } = config;
        let config = crate::parser::ProviderConfig {
            name: name.clone(),
            attributes: project_attributes(attributes),
            default_tags: project_attributes(default_tags),
            source: source.clone(),
            version: version.clone(),
            revision: revision.clone(),
            unresolved_attributes: project_attributes(unresolved_attributes),
            binding: binding.clone(),
        };
        // This is the existing host merge, also called by plan preprocessing
        // and dispatch normalization before WireReady admission (#4279).
        crate::provider::merge_default_tags(&config, &mut resources, schemas)?;
    }
    let mut findings = Vec::new();
    for resource in &resources {
        if let Some(site) =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(resource), schemas)
        {
            for rule in site.applicable_rules() {
                if let InputVerdict::Violation(violation) = rule.evaluate() {
                    findings.push(AuthoredInputFinding {
                        resource_id: resource.id.clone(),
                        violation,
                    });
                }
            }
        }
    }
    Ok(findings)
}

fn project_attributes<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
    attributes: &IndexMap<String, Value<P, B>>,
) -> IndexMap<String, Value> {
    attributes
        .iter()
        .map(|(key, value)| (key.clone(), project_known_value(value)))
        .collect()
}

/// Erase unresolved expressions conservatively, retaining known container keys
/// and secret protection. No read-back value or state is consulted here.
fn project_known_value<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
    value: &Value<P, B>,
) -> Value {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => {
            Value::Concrete(ConcreteValue::String(value.clone()))
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
            Value::Concrete(ConcreteValue::CanonicalEnum(value.clone()))
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            Value::Concrete(ConcreteValue::EnumIdentifier(value.clone()))
        }
        Value::Concrete(ConcreteValue::Int(value)) => Value::Concrete(ConcreteValue::Int(*value)),
        Value::Concrete(ConcreteValue::Float(value)) => {
            Value::Concrete(ConcreteValue::Float(*value))
        }
        Value::Concrete(ConcreteValue::Bool(value)) => Value::Concrete(ConcreteValue::Bool(*value)),
        Value::Concrete(ConcreteValue::Duration(value)) => {
            Value::Concrete(ConcreteValue::Duration(*value))
        }
        Value::Concrete(ConcreteValue::StringList(value)) => {
            Value::Concrete(ConcreteValue::StringList(value.clone()))
        }
        Value::Concrete(ConcreteValue::List(values)) => Value::Concrete(ConcreteValue::List(
            values.iter().map(project_known_value).collect(),
        )),
        Value::Concrete(ConcreteValue::Map(entries)) => {
            Value::Concrete(ConcreteValue::Map(project_attributes(entries)))
        }
        Value::Deferred(DeferredValue::Secret(secret)) => {
            project_known_value(secret.expose_plaintext()).into_secret()
        }
        Value::Deferred(_) => Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
    }
}

/// Every desired write slot either passed or retains an explicit obligation.
/// Cloning this immutable owner preserves the same effects; mutable/raw plans
/// cannot recover its execution authority.
///
/// ```compile_fail
/// use carina_core::{input_rules::InputCheckedPlan, plan::Plan};
/// let checked: InputCheckedPlan = Plan::new().into();
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::InputCheckedPlan, resource::Resource};
/// let checked: InputCheckedPlan = Resource::new("test", "r").into();
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::InputCheckedPlan, effect::Effect};
/// fn forge(effect: Effect) -> InputCheckedPlan { effect.into() }
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::InputCheckedPlan, plan::Plan};
/// let checked = InputCheckedPlan { plan: Plan::new(), pending: Vec::new() };
/// ```
/// ```compile_fail
/// use carina_core::input_rules::InputCheckedPlan;
/// let checked: InputCheckedPlan = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::{input_rules::InputCheckedPlan, effect::Effect};
/// fn mutate(checked: &mut InputCheckedPlan, effect: Effect) { checked.add(effect); }
/// ```
/// ```compile_fail
/// use carina_core::input_rules::InputCheckedPlan;
/// let checked = InputCheckedPlan::default();
/// ```
#[derive(Debug, Clone)]
pub struct InputCheckedPlan<S = ModuleSource> {
    plan: Plan<S>,
    obligations: Box<InputObligations>,
}

#[derive(Debug, Clone)]
struct InputObligations {
    pending: Vec<PendingInputObligation>,
    write_checks: Vec<PendingInputObligation>,
}

impl<S> Deref for InputCheckedPlan<S> {
    type Target = Plan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> InputCheckedPlan<S> {
    pub(crate) fn execution_input_checks(&self) -> Vec<PendingInputObligation> {
        self.obligations.write_checks.clone()
    }
    pub fn plan(&self) -> &Plan<S> {
        &self.plan
    }
    pub fn pending_inputs(&self) -> &[PendingInputObligation] {
        &self.obligations.pending
    }
    pub fn ensure_inputs_ready(&self) -> Result<(), InputCheckPending> {
        if self.obligations.pending.is_empty() {
            Ok(())
        } else {
            Err(InputCheckPending)
        }
    }
    pub(crate) fn pending_rows(&self, effect_index: usize) -> Vec<crate::detail_rows::DetailRow> {
        let mut paths: BTreeMap<InputPath, Vec<InputRuleCode>> = BTreeMap::new();
        for obligation in &self.obligations.pending {
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
                crate::detail_rows::DetailRow::PendingInput { path, codes }
            })
            .collect()
    }
}

/// Only executor discharge can supply this unforgeable constructor token.
///
/// ```compile_fail
/// use carina_core::input_rules::executor_write_site;
/// ```
pub(crate) fn executor_write_site<'a>(
    authority: &'a crate::executor::input_discharge::DischargeToken,
    admission: &'a PendingInputObligation,
    resource_id: &'a ResourceId,
    attributes: &'a IndexMap<String, Value>,
    schemas: &'a SchemaRegistry,
) -> Option<WriteSite<'a>> {
    classify_write_site::<ModuleSource>(
        WriteSiteSource::Executor {
            authority,
            admission,
            resource_id,
            attributes,
        },
        schemas,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("input check pending; apply-time discharge not implemented")]
pub struct InputCheckPending;

fn evaluate_string(rule: &InputRule<Compiled>, value: &str) -> Result<(), InputViolation> {
    let (accepted, reason) = match rule.predicate() {
        InputPredicate::Length { min, max } => {
            let count = u64::try_from(value.chars().count()).expect("string length fits u64");
            (
                min.is_none_or(|min| count >= min) && max.is_none_or(|max| count <= max),
                InputViolationReason::Length,
            )
        }
        InputPredicate::AllowedChars(set) => (
            value.chars().all(|ch| set.contains(ch)),
            InputViolationReason::AllowedChars,
        ),
        InputPredicate::FirstChar(set) => (
            value.chars().next().is_some_and(|ch| set.contains(ch)),
            InputViolationReason::FirstChar,
        ),
        InputPredicate::LastChar(set) => (
            value.chars().next_back().is_some_and(|ch| set.contains(ch)),
            InputViolationReason::LastChar,
        ),
        InputPredicate::NoSubstring(token) => (
            !value.contains(token.as_str()),
            InputViolationReason::NoSubstring,
        ),
        InputPredicate::NoPrefix(token) => (
            !value.starts_with(token.as_str()),
            InputViolationReason::NoPrefix,
        ),
        InputPredicate::ForbiddenPrefix {
            prefix,
            ignore_ascii_case,
        } => (
            !value.get(..prefix.as_str().len()).is_some_and(|head| {
                if *ignore_ascii_case {
                    head.eq_ignore_ascii_case(prefix.as_str())
                } else {
                    head == prefix.as_str()
                }
            }),
            InputViolationReason::ForbiddenPrefix,
        ),
        InputPredicate::NoSuffix(token) => (
            !value.ends_with(token.as_str()),
            InputViolationReason::NoSuffix,
        ),
        InputPredicate::NoAdjacent { left, right } => (
            !value
                .chars()
                .zip(value.chars().skip(1))
                .any(|(a, b)| left.contains(a) && right.contains(b)),
            InputViolationReason::NoAdjacent,
        ),
        InputPredicate::NotIpv4 => (!is_ipv4(value), InputViolationReason::NotIpv4),
        InputPredicate::Matches(pattern) => {
            (pattern.is_match(value), InputViolationReason::Matches)
        }
        InputPredicate::NotMatches(pattern) => {
            (!pattern.is_match(value), InputViolationReason::NotMatches)
        }
    };
    if accepted {
        Ok(())
    } else {
        Err(InputViolation {
            path: rule.path().clone(),
            code: rule.code().clone(),
            reason,
        })
    }
}
/// Strict decimal dotted quad: exactly four ASCII octets in 0..=255, without
/// signs, whitespace or leading zeroes on multi-digit octets.
fn is_ipv4(value: &str) -> bool {
    let mut octets = value.split('.');
    for _ in 0..4 {
        let Some(octet) = octets.next() else {
            return false;
        };
        if octet.is_empty()
            || octet.len() > 3
            || (octet.len() > 1 && octet.starts_with('0'))
            || !octet.bytes().all(|byte| byte.is_ascii_digit())
            || octet.parse::<u8>().is_err()
        {
            return false;
        }
    }
    octets.next().is_none()
}
