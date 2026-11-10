//! Scoped desired-input admission after effect finalization. Only this module
//! classifies write sites and evaluates rules. Witnesses are never persisted.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Deref,
    sync::{Arc, OnceLock},
};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

mod authored;
pub use authored::{AuthoredInputUnavailable, AuthoredInputs};
mod declarations;
mod estimator;
mod instances;
mod rule_input;
mod type_context;
mod union_fits;
use type_context::TypeFacts;
pub use type_context::{InputCompositions, InputTypeContext};
pub(crate) use union_fits::{InputSelectionAuthority, InputUnionAdmission};

#[cfg(test)]
mod admission_budget_tests;
#[cfg(test)]
mod boundary_tests;
#[cfg(test)]
mod condition_tests;
#[cfg(test)]
mod estimator_tests;
#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod seam_tests;

use crate::{
    effect::{Effect, EffectGeneration, EffectInstanceKey},
    plan::{Plan, PlanError, PlanErrorKind},
    resource::{
        ConcreteValue, Current, DeferredValue, ModuleSource, ParserPlaceholder, ResourceId, Value,
        ValueKind,
    },
    schema::{
        Compiled, CompiledAttributeType, InputCondition, InputDiagnosticPath, InputLiteral,
        InputMode, InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope,
        InputRuleTarget, InputViolation, SchemaKind, SchemaRegistry, ValidatedResourceSchema,
    },
};

// One transient input phase retains parser and loop substitutions without
// granting them provider-readiness or changing their knowledge provenance.
type InputValue = Value<ParserPlaceholder>;
type InputAttributes = IndexMap<String, InputValue>;
pub(crate) use declarations::InputRuleDeclarations;
pub use rule_input::{AbsenceProof, InputRead, NodeRef, PatchRead, RuleInput, WithheldReason};
use rule_input::{InstanceRef, Membership};

// Only this classified write-site owner can open protected input evidence.
pub(crate) struct InputEvidenceAuthority(());

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
}

impl InputEffectTarget {
    pub fn effect_index(&self) -> usize {
        self.effect_index
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
/// ```compile_fail,E0277
/// use carina_core::{input_rules::WriteSite, resource::Resource};
/// let view: WriteSite<'_> = Resource::new("test", "r").into();
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::WriteSite, effect::Effect};
/// fn forge(effect: &Effect) -> WriteSite<'_> { effect.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::WriteSite, effect::Effect};
/// fn forge(effect: Effect) -> WriteSite<'static> { effect.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::input_rules::WriteSite;
/// let view: WriteSite<'_> = Default::default();
/// ```
/// ```compile_fail,E0277
/// use carina_core::input_rules::WriteSite;
/// let view: WriteSite<'_> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0616
/// use carina_core::input_rules::WriteSite;
/// fn mutate(view: &mut WriteSite<'_>) { view.input = todo!(); }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::WriteSite, resource::Current};
/// fn forge(current: &Current) -> WriteSite<'_> { current.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::WriteSite, resource::State};
/// fn forge(state: &State) -> WriteSite<'_> { state.into() }
/// ```
/// ```compile_fail,E0451
/// use carina_core::{input_rules::{ApplicableInputRule, WriteSite}, schema::{Compiled, InputRule}};
/// fn bypass<'a>(site: &WriteSite<'a>, rule: &'a InputRule<Compiled>) {
///     let applicable = ApplicableInputRule { binding: todo!() };
///     let _ = applicable.evaluate();
/// }
/// ```
/// The executor constructor requires discharge authority and is inaccessible
/// outside core.
///
/// ```compile_fail,E0603
/// use carina_core::input_rules::executor_write_site;
/// use carina_core::executor::input_discharge::DischargeToken;
/// ```
pub struct WriteSite<'a> {
    target: Option<InputEffectTarget>,
    resource_id: ResourceId,
    input: Arc<RuleInput>,
    context: InputWriteContext,
    schema: &'a ValidatedResourceSchema,
    rules: Vec<&'a InputRule<Compiled>>,
    paired_deletes: Vec<PairedInputDelete>,
}

impl<'a> WriteSite<'a> {
    pub fn target(&self) -> Option<&InputEffectTarget> {
        self.target.as_ref()
    }
    pub fn applicable_rules<'s>(&'s self) -> impl Iterator<Item = ApplicableInputRule<'s, 'a>> {
        instances::for_site(self).into_iter()
    }

    /// Evaluate every applicable declaration through the single rule evaluator.
    /// Successful rules never materialize a diagnostic path.
    pub fn evaluate(&self) -> InputEvaluation {
        let mut complete = InputEvaluation::empty();
        for rule in self.applicable_rules() {
            let InputEvaluation { findings, pending } = rule.evaluate();
            complete.findings.extend(findings);
            complete.pending.extend(pending);
        }
        complete.findings.sort_by(|a, b| {
            (a.violation.path(), a.violation.code()).cmp(&(b.violation.path(), b.violation.code()))
        });
        complete.pending.sort();
        complete
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }
    pub fn context(&self) -> InputWriteContext {
        self.context
    }
}

/// An applicable declaration bound to the desired projection selected by the
/// classifier. Callers supply neither a rule nor a value when evaluating it.
pub struct ApplicableInputRule<'s, 'a> {
    binding: InputRuleBinding<'s, 'a>,
}

enum InputRuleBinding<'s, 'a> {
    Instance(BoundInputRule<'s, 'a>),
    // One obligation for unknown membership. Declarations are retained once,
    // and the ordinary instance walker binds concrete descendants at dispatch.
    DeferredSubtree {
        first: BoundInputRule<'s, 'a>,
        remaining: Vec<BoundInputRule<'s, 'a>>,
    },
}

/// Complete typed results for one binding or an entire write site. There is
/// no public conversion to the lossy, single-rule verdict used internally.
#[derive(Debug, Clone)]
pub struct InputEvaluation {
    findings: Vec<InputFinding>,
    pending: Vec<(InputDiagnosticPath, InputRuleCode)>,
}

#[derive(Debug, Clone)]
pub enum InputEvaluationOutcome {
    Finding(Box<InputFinding>),
    Pending {
        path: InputDiagnosticPath,
        code: InputRuleCode,
    },
}

impl InputEvaluation {
    fn empty() -> Self {
        Self {
            findings: Vec::new(),
            pending: Vec::new(),
        }
    }
    pub fn findings(&self) -> &[InputFinding] {
        &self.findings
    }
    pub fn pending(&self) -> &[(InputDiagnosticPath, InputRuleCode)] {
        &self.pending
    }
    pub fn is_proven(&self) -> bool {
        self.findings.is_empty() && self.pending.is_empty()
    }
    pub fn into_outcomes(self) -> impl Iterator<Item = InputEvaluationOutcome> {
        let Self { findings, pending } = self;
        findings
            .into_iter()
            .map(|finding| InputEvaluationOutcome::Finding(Box::new(finding)))
            .chain(
                pending
                    .into_iter()
                    .map(|(path, code)| InputEvaluationOutcome::Pending { path, code }),
            )
    }
}

/// A nonempty complete failure, retaining every established finding and
/// unresolved obligation when one provider operation cannot be dispatched.
#[derive(Debug, Clone)]
pub struct InputEvaluationFailure {
    first: InputEvaluationOutcome,
    remaining: Vec<InputEvaluationOutcome>,
}
impl InputEvaluationFailure {
    pub fn outcomes(&self) -> impl Iterator<Item = &InputEvaluationOutcome> {
        std::iter::once(&self.first).chain(&self.remaining)
    }
    pub fn has_findings(&self) -> bool {
        self.outcomes()
            .any(|outcome| matches!(outcome, InputEvaluationOutcome::Finding(_)))
    }
}
impl std::fmt::Display for InputEvaluationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, outcome) in self.outcomes().enumerate() {
            if index > 0 {
                f.write_str("\n")?;
            }
            match outcome {
                InputEvaluationOutcome::Finding(finding) => write!(
                    f,
                    "{} [{}]: {}",
                    finding.violation.path(),
                    finding.violation.code(),
                    finding.message()
                )?,
                InputEvaluationOutcome::Pending { path, code } => {
                    write!(f, "input rule {code} at {path} could not be discharged")?
                }
            }
        }
        Ok(())
    }
}
impl std::error::Error for InputEvaluationFailure {}

impl InputEvaluation {
    pub(crate) fn into_dispatch_failure(self) -> Option<crate::executor::EffectFailureKind> {
        use crate::executor::EffectFailureKind;
        let mut outcomes = self.into_outcomes();
        let first = outcomes.next()?;
        let remaining = outcomes.collect::<Vec<_>>();
        Some(if remaining.is_empty() {
            match first {
                InputEvaluationOutcome::Finding(finding) => EffectFailureKind::InputRule {
                    violation: finding.violation,
                },
                InputEvaluationOutcome::Pending { path, code } => {
                    EffectFailureKind::UnresolvedInputRule { path, code }
                }
            }
        } else {
            EffectFailureKind::InputRuleEvaluation {
                failure: Box::new(InputEvaluationFailure { first, remaining }),
            }
        })
    }
}

impl<'s, 'a> ApplicableInputRule<'s, 'a> {
    fn instance(rule: BoundInputRule<'s, 'a>) -> Self {
        Self {
            binding: InputRuleBinding::Instance(rule),
        }
    }
    fn deferred(rules: Vec<BoundInputRule<'s, 'a>>) -> Option<Self> {
        let mut rules = rules.into_iter();
        let first = rules.next()?;
        Some(Self {
            binding: InputRuleBinding::DeferredSubtree {
                first,
                remaining: rules.collect(),
            },
        })
    }
    pub fn evaluate(&self) -> InputEvaluation {
        let (first, remaining) = match &self.binding {
            InputRuleBinding::Instance(rule) => (rule, &[][..]),
            InputRuleBinding::DeferredSubtree { first, remaining } => (first, remaining.as_slice()),
        };
        let mut evaluation = InputEvaluation::empty();
        for rule in std::iter::once(first).chain(remaining) {
            match rule.evaluate() {
                InputVerdict::Proven => {}
                InputVerdict::Violation(violation) => {
                    evaluation.findings.push(rule.finding(violation))
                }
                InputVerdict::Undecided => {
                    // Unknown descendant membership retains one subtree
                    // obligation while independent established failures survive.
                    if evaluation.pending.is_empty() {
                        evaluation
                            .pending
                            .push((rule.path().clone(), rule.code().clone()));
                    }
                }
            }
        }
        evaluation
    }
    #[cfg(test)]
    fn test_verdicts(&self) -> Vec<InputVerdict> {
        let evaluation = self.evaluate();
        if evaluation.is_proven() {
            return vec![InputVerdict::Proven];
        }
        let mut verdicts = evaluation
            .findings
            .into_iter()
            .map(|finding| InputVerdict::Violation(finding.violation))
            .collect::<Vec<_>>();
        verdicts.extend(
            evaluation
                .pending
                .into_iter()
                .map(|_| InputVerdict::Undecided),
        );
        verdicts
    }
    #[cfg(test)]
    fn verdict_for_test(&self) -> InputVerdict {
        let mut verdicts = self.test_verdicts();
        assert_eq!(
            verdicts.len(),
            1,
            "single-rule fixture must inspect all outcomes for a multi-rule binding"
        );
        verdicts.pop().unwrap()
    }
}

struct BoundInputRule<'s, 'a> {
    site: &'s WriteSite<'a>,
    rule: &'a InputRule<Compiled>,
    kind: InputFindingKind,
    owner_type: Option<&'a CompiledAttributeType>,
    path: OnceLock<InputDiagnosticPath>,
    owner: instances::OwnerPath,
    pending_instance: bool,
    instance: InstanceRef,
}

impl BoundInputRule<'_, '_> {
    pub(crate) fn path(&self) -> &InputDiagnosticPath {
        self.path
            .get_or_init(|| instances::diagnostic_path(&self.owner, self.rule.path()))
    }
    pub(crate) fn code(&self) -> &InputRuleCode {
        self.rule.code()
    }
    fn evaluate(&self) -> InputVerdict {
        let verdict = verdict(self);
        match verdict {
            InputVerdict::Violation(mut violation) => {
                violation.path = self.path().clone();
                InputVerdict::Violation(violation)
            }
            InputVerdict::Proven | InputVerdict::Undecided => verdict,
        }
    }

    fn finding(&self, violation: InputViolation) -> InputFinding {
        InputFinding {
            resource_id: self.site.resource_id.clone(),
            violation,
            kind: self.kind,
            declaration: self.site.input.declaration().clone(),
        }
    }
}

enum WriteSiteSource<'a, S> {
    Finalized {
        plan: &'a FinalizedPlan<S>,
        index: usize,
        current: &'a HashMap<ResourceId, Current>,
        types: Option<Arc<TypeFacts>>,
    },
    #[cfg(test)]
    Authored(&'a crate::resource::Resource),
    Preview {
        resource_id: ResourceId,
        input: Arc<RuleInput>,
    },
    Executor {
        #[allow(dead_code)] // Type-level construction authority; no runtime data.
        authority: &'a crate::executor::input_discharge::DischargeToken,
        admission: &'a PendingInputObligation,
        input: Arc<RuleInput>,
    },
}

/// The sole classifier and constructor of desired write sites. Current is
/// consulted only for the issued/unissued continuation distinction; its values
/// never become authored inputs. Updates select the patch's sent values,
/// including any ignored readable leaves preserved by patch construction.
fn classify_write_site<'a, S>(
    source: WriteSiteSource<'a, S>,
    schemas: &'a SchemaRegistry,
) -> Result<Option<WriteSite<'a>>, crate::provider::UpdatePatchError> {
    let (target, resource_id, input, context, paired_deletes, schema) = match source {
        WriteSiteSource::Executor {
            authority: _,
            admission,
            input,
        } => (
            Some(admission.target.clone()),
            input.resource_id().clone(),
            input,
            admission.context,
            admission.paired_deletes.clone(),
            admission.schema.as_ref(),
        ),
        #[cfg(test)]
        WriteSiteSource::Authored(resource) => {
            let Some(schema) = schemas.get_for(resource) else {
                return Ok(None);
            };
            let schema = schema.schema();
            let authored = AuthoredInputs::resource_for_test(resource);
            let source = authored.resources().next().unwrap();
            let input = RuleInput::for_authored(&source, schema, Arc::new(TypeFacts::default()))
                .expect("fixture canonicalization");
            (
                None,
                resource.id.clone(),
                input,
                InputWriteContext::AuthoredDesired,
                Vec::new(),
                schema,
            )
        }
        WriteSiteSource::Preview { resource_id, input } => {
            let Some(schema) = schemas.get(
                &resource_id.provider,
                &resource_id.resource_type,
                SchemaKind::Resource,
            ) else {
                return Ok(None);
            };
            (
                None,
                resource_id,
                input,
                InputWriteContext::AuthoredDesired,
                Vec::new(),
                schema.schema(),
            )
        }
        WriteSiteSource::Finalized {
            plan,
            index,
            current,
            types,
        } => {
            let effect = &plan.plan.effects()[index];
            let Some(schema) = schemas.get(
                &effect.resource_id().provider,
                &effect.resource_id().resource_type,
                SchemaKind::Resource,
            ) else {
                return Ok(None);
            };
            let schema = schema.schema();
            let declarations = types.unwrap_or_else(|| Arc::new(TypeFacts::default()));
            let paired = || {
                plan.replacement_creates
                    .get(&index)
                    .into_iter()
                    .flat_map(|info| &info.delete_indices)
                    .map(|&delete_index| PairedInputDelete {
                        plan: plan.identity.clone(),
                        effect_index: delete_index,
                        absorbed_index: None,
                        instance: plan.plan.effects()[delete_index].instance_key(),
                    })
                    .collect()
            };
            let (input, context, deletes) = match effect {
                Effect::Create(resource) => (
                    RuleInput::for_plan_create(resource, schema, declarations),
                    plan.replacement_creates.get(&index).map_or(
                        InputWriteContext::Create,
                        |info| InputWriteContext::Replacement {
                            create_before_destroy: info.create_before_destroy,
                        },
                    ),
                    paired(),
                ),
                Effect::Update {
                    from,
                    to,
                    changed_attributes,
                    identifier: _,
                } => (
                    RuleInput::for_plan_update(to, from, changed_attributes, schema, declarations)?,
                    InputWriteContext::Update,
                    Vec::new(),
                ),
                Effect::ResumeCreate {
                    resource,
                    started_at: _,
                } => {
                    if matches!(current.get(&resource.id), Some(Current::CreateInFlight(pending)) if pending.identifier().is_some())
                    {
                        return Ok(None);
                    }
                    (
                        RuleInput::for_plan_create(resource, schema, declarations),
                        InputWriteContext::ResumeCreate,
                        paired(),
                    )
                }
                Effect::DeferredCreate(payload) => (
                    RuleInput::for_plan_deferred_create(
                        payload.template.template_resource(),
                        schema,
                        declarations,
                    ),
                    InputWriteContext::DeferredCreate,
                    Vec::new(),
                ),
                Effect::DeferredReplace(payload) => (
                    RuleInput::for_plan_deferred_replace(
                        payload.template.template_resource(),
                        schema,
                        declarations,
                    ),
                    InputWriteContext::DeferredReplacement,
                    payload
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
                        .collect(),
                ),
                Effect::Read { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. } => return Ok(None),
            };
            (
                Some(InputEffectTarget {
                    plan: plan.identity.clone(),
                    effect_index: index,
                }),
                effect.resource_id().clone(),
                input,
                context,
                deletes,
                schema,
            )
        }
    };
    let mut rules = schema.as_schema().input_rules().iter().collect::<Vec<_>>();
    rules.sort_by(|a, b| (a.target(), a.code()).cmp(&(b.target(), b.code())));
    Ok(Some(WriteSite {
        target,
        resource_id,
        input,
        context,
        schema,
        rules,
        paired_deletes,
    }))
}

// Applicability and finding kind are one decision for both declaration owners.
fn scope_classification(
    scope: InputRuleScope,
    context: InputWriteContext,
) -> Option<InputFindingKind> {
    match scope {
        InputRuleScope::CreateOnly => match context {
            InputWriteContext::Create
            | InputWriteContext::Replacement {
                create_before_destroy: _,
            }
            | InputWriteContext::ResumeCreate
            | InputWriteContext::DeferredCreate
            | InputWriteContext::DeferredReplacement => Some(InputFindingKind::Violation),
            InputWriteContext::AuthoredDesired => Some(InputFindingKind::CreateOnlyPreview),
            InputWriteContext::Update => None,
        },
        InputRuleScope::EveryWrite => match context {
            InputWriteContext::Create
            | InputWriteContext::Replacement {
                create_before_destroy: _,
            }
            | InputWriteContext::ResumeCreate
            | InputWriteContext::DeferredCreate
            | InputWriteContext::DeferredReplacement
            | InputWriteContext::Update
            | InputWriteContext::AuthoredDesired => Some(InputFindingKind::Violation),
        },
    }
}

/// An explicit remainder of admission; it cannot be deserialized or discharged
/// by callers. Deferred slots bind to the template effect, never a name.
///
/// ```compile_fail,E0277
/// use carina_core::input_rules::PendingInputObligation;
/// let pending: PendingInputObligation = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0277
/// use carina_core::input_rules::PendingInputObligation;
/// fn persist(pending: &PendingInputObligation) { let _ = serde_json::to_string(pending); }
/// ```
#[derive(Debug, Clone)]
pub struct PendingInputObligation {
    target: InputEffectTarget,
    pending: Vec<(InputDiagnosticPath, InputRuleCode)>,
    paired_deletes: Vec<PairedInputDelete>,
    context: InputWriteContext,
    pub(crate) schema: Arc<ValidatedResourceSchema>,
}

impl PendingInputObligation {
    pub fn target(&self) -> &InputEffectTarget {
        &self.target
    }
    pub fn paired_deletes(&self) -> &[PairedInputDelete] {
        &self.paired_deletes
    }
}

/// Finalization has completed all effect rewrites. There is no public raw-plan
/// constructor or mutable access; input admission consumes this owner.
///
/// ```compile_fail,E0277
/// use carina_core::{input_rules::FinalizedPlan, plan::Plan};
/// let finalized: FinalizedPlan = Plan::new().into();
/// ```
/// ```compile_fail,E0599
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
    delete_indices: Vec<usize>,
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

/// Explicit fixture-only bridge from a hand-built plan into execution admission.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn finalize_plan_for_test<S>(plan: Plan<S>) -> FinalizedPlan<S> {
    FinalizedPlan::new(plan)
}

impl FinalizedPlan<crate::resource::SavedModuleSource> {
    /// Seal the saved effects after the caller has verified and rehydrated
    /// them. This performs no diff or effect rewrite and grants no execution
    /// authority until the ordinary input-admission transition succeeds.
    pub(crate) fn from_saved_plan(plan: Plan<crate::resource::SavedModuleSource>) -> Self {
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
                        delete_indices: vec![info.delete_idx],
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
    ) -> impl Iterator<Item = Result<WriteSite<'a>, crate::provider::UpdatePatchError>> {
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
            .transpose()
        })
    }

    /// Admit every final desired write and resend-capable continuation. Existing
    /// diagnostics are retained; new diagnostics are sorted by routed address,
    /// path and code independently of declaration order.
    pub fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>>
    where
        S: Clone,
    {
        self.check_inputs_with_types(schemas, current, &InputTypeContext::empty())
    }

    /// Admit using the one immutable declaration context retained for this plan.
    pub fn check_inputs_with_types(
        mut self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
        types: &InputTypeContext<S>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>>
    where
        S: Clone,
    {
        // Replacement admission may install prepared temporary names. Classify
        // the resulting exact effects, never the draft that preceded that seam.
        let admitted = match self.plan.clone().admit(schemas) {
            Ok(plan) => plan,
            Err(error) => {
                let error = error.into_plan_error();
                if !self.plan.errors().contains(&error) {
                    self.plan.add_error(error);
                }
                return Err(InputAdmissionFailure {
                    plan: Box::new(self.plan),
                });
            }
        };
        self = Self::new(admitted.clone().into_unadmitted());
        self.replacement_creates = admitted
            .admitted_effects()
            .input_replacement_sites()
            .into_iter()
            .map(|(index, delete_indices, create_before_destroy)| {
                (
                    index,
                    ReplacementWrite {
                        delete_indices,
                        create_before_destroy,
                    },
                )
            })
            .collect();
        let mut diagnostics = Vec::new();
        let mut pending = Vec::new();
        let mut write_checks = Vec::new();
        let declarations = Arc::new(types.facts.clone());
        for index in 0..self.plan.effects().len() {
            let view = match classify_write_site(
                WriteSiteSource::Finalized {
                    plan: &self,
                    index,
                    current,
                    types: Some(Arc::clone(&declarations)),
                },
                schemas,
            ) {
                Ok(Some(view)) => view,
                Ok(None) => continue,
                Err(error) => {
                    self.plan.add_error(PlanError::new(
                        self.plan.effects()[index].resource_id().clone(),
                        error.into(),
                    ));
                    continue;
                }
            };
            if !instances::has_declarations(&view) {
                continue;
            }
            let context = view.context;
            let mut undecided = Vec::new();
            let schema = Arc::new(view.schema.clone());
            // Normalization can change even statically proven values. Retain
            // the classified authority for rechecking the exact wire request.
            write_checks.push(PendingInputObligation {
                target: view.target.clone().expect("finalized slot"),
                pending: Vec::new(),
                paired_deletes: view.paired_deletes.clone(),
                context,
                schema: Arc::clone(&schema),
            });
            let InputEvaluation {
                findings,
                pending: unresolved,
            } = view.evaluate();
            diagnostics.extend(findings.into_iter().map(|finding| (context, finding)));
            undecided.extend(unresolved);
            if !undecided.is_empty() {
                pending.push(PendingInputObligation {
                    target: view
                        .target
                        .expect("finalized sites carry an exact effect slot"),
                    pending: undecided,
                    paired_deletes: view.paired_deletes,
                    context,
                    schema: Arc::clone(&schema),
                });
            }
        }
        // Include provider-instance routing, which display spelling omits.
        diagnostics.sort_by(|(_, a), (_, b)| {
            let av = &a.violation;
            let bv = &b.violation;
            let a = &a.resource_id;
            let b = &b.resource_id;
            (
                &a.provider,
                &a.provider_instance,
                &a.resource_type,
                a.identity_str(),
                av.path(),
                av.code(),
            )
                .cmp(&(
                    &b.provider,
                    &b.provider_instance,
                    &b.resource_type,
                    b.identity_str(),
                    bv.path(),
                    bv.code(),
                ))
        });
        for (context, finding) in diagnostics {
            let violation = &finding.violation;
            self.plan.add_error(PlanError::new(
                finding.resource_id.clone(),
                PlanErrorKind::InputRule {
                    path: violation.path().as_str().into(),
                    code: violation.code().as_str().into(),
                    reason: finding.message().to_string(),
                    context,
                },
            ));
        }
        if self.plan.has_errors() {
            Err(InputAdmissionFailure {
                plan: Box::new(self.plan),
            })
        } else {
            Ok(InputCheckedPlan {
                plan: admitted,
                obligations: Box::new(InputObligations {
                    pending,
                    write_checks,
                }),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputVerdict {
    Proven,
    Violation(InputViolation),
    Undecided,
}

#[derive(Clone, Copy)]
enum Truth {
    True,
    False,
    Pending,
    Failed,
}

#[cfg(test)]
thread_local! { static DECLARATION_LOOKUP_WORK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn record_declaration_lookup_work(count: usize) {
    DECLARATION_LOOKUP_WORK.with(|work| {
        if let Some(current) = work.get() {
            work.set(Some(current + count));
        }
    });
}

fn presence(evidence: InputRead<'_>) -> Truth {
    match evidence {
        InputRead::Absent(_) => Truth::False,
        InputRead::Withheld(_) => Truth::Pending,
        InputRead::Failed(_) => Truth::Failed,
        InputRead::Present(value) => {
            if value.failed() {
                Truth::Failed
            } else {
                Truth::True
            }
        }
    }
}

fn path_evidence<'s>(applicable: &'s BoundInputRule<'_, '_>, path: &InputPath) -> InputRead<'s> {
    applicable.site.input.read(applicable.instance, path)
}

fn literal_type<'a>(
    applicable: &BoundInputRule<'_, 'a>,
    path: &InputPath,
) -> Option<&'a CompiledAttributeType> {
    input_declaration(applicable, path, path.segments().count()).map(|(ty, _)| ty)
}

/// The declaration owner supplies both scalar identity and omission policy.
fn input_declaration<'a>(
    applicable: &BoundInputRule<'_, 'a>,
    path: &InputPath,
    count: usize,
) -> Option<(&'a CompiledAttributeType, InputMode)> {
    use crate::schema::AttrTypeKind;
    let defs = applicable.site.schema.as_schema().defs();
    let mut segments = path.segments().take(count);
    let (mut ty, mut mode) = match applicable.owner_type {
        Some(ty) => (ty, InputMode::Required),
        None => {
            let attribute = applicable
                .site
                .schema
                .as_schema()
                .attributes
                .get(segments.next()?)?;
            (&attribute.attr_type, attribute.input_mode)
        }
    };
    for segment in segments {
        ty = ty.resolve_refs_with_defs(defs).as_attr();
        let AttrTypeKind::Struct { fields, .. } = &ty.kind else {
            return None;
        };
        let field = fields.field(segment)?;
        ty = &field.field_type;
        mode = field.parts().declared_input_mode;
    }
    Some((ty.resolve_refs_with_defs(defs).as_attr(), mode))
}

/// Known scalar evidence is canonical once, regardless of predicate or owner.
/// Invalid evidence cannot fall through to conservative expression estimation.
enum ScalarEvidence {
    Known(InputLiteral),
    Pending,
    Failed,
}

fn scalar_value<'a>(
    applicable: &BoundInputRule<'_, 'a>,
    path: &InputPath,
    evidence: &NodeRef<'_>,
) -> ScalarEvidence {
    use crate::resource::{
        EnumValueResolver,
        knowledge::{Classification, KnownView},
    };
    if evidence.failed() {
        return ScalarEvidence::Failed;
    }
    if !evidence.known() {
        return ScalarEvidence::Pending;
    }
    let Some(Classification::Known(known)) = evidence.proof() else {
        return ScalarEvidence::Pending;
    };
    let defs = applicable.site.schema.as_schema().defs();
    let Some(mut ty) = literal_type(applicable, path) else {
        return ScalarEvidence::Failed;
    };
    while matches!(ty.kind(), crate::schema::AttrTypeKind::Union(_)) {
        match evidence.union_selection(ty) {
            union_fits::UnionSelection::Selected(index) => {
                let crate::schema::RawShape::Union(members) = ty.raw_shape() else {
                    unreachable!("union declaration")
                };
                ty = members[index].resolve_refs_with_defs(defs).as_attr();
            }
            union_fits::UnionSelection::Withheld => return ScalarEvidence::Pending,
            union_fits::UnionSelection::Rejected => return ScalarEvidence::Failed,
        };
    }
    let resolver = EnumValueResolver::with_defs(ty, defs);
    let enum_literal = |result: Result<
        crate::resource::CanonicalEnumValue,
        crate::schema::TypeError,
    >| match result {
        Ok(value) => ScalarEvidence::Known(InputLiteral::Enum {
            identity: value.identity().clone(),
            api_value: value.api_value().into(),
        }),
        Err(_) => ScalarEvidence::Failed,
    };
    known.visit(
        |value| match value {
            KnownView::Bool(value) => ScalarEvidence::Known(InputLiteral::Bool(value)),
            KnownView::Int(value) => ScalarEvidence::Known(InputLiteral::Int(value)),
            KnownView::Float(value) => ScalarEvidence::Known(InputLiteral::Float(value)),
            KnownView::CanonicalEnum(value) => {
                if resolver.enum_identity().as_ref() != Some(value.identity()) {
                    return ScalarEvidence::Failed;
                }
                enum_literal(resolver.resolve_state_text(value.api_value()))
            }
            KnownView::EnumIdentifier(value) => enum_literal(resolver.resolve_raw(value)),
            KnownView::String(value) => {
                if ty.enum_parts().is_some() {
                    enum_literal(resolver.resolve_state_text(value.expose_plaintext()))
                } else {
                    ScalarEvidence::Known(InputLiteral::String(value.expose_plaintext().into()))
                }
            }
            KnownView::Duration(_)
            | KnownView::List(_)
            | KnownView::StringList(_)
            | KnownView::Map(_) => ScalarEvidence::Failed,
        },
        |_| ScalarEvidence::Failed,
    )
}

/// Guard and target equality share this sole canonical scalar comparison.
fn scalar_equals(
    applicable: &BoundInputRule<'_, '_>,
    path: &InputPath,
    literal: &InputLiteral,
    evidence: InputRead<'_>,
) -> Truth {
    let evidence = match evidence {
        InputRead::Present(value) => value,
        InputRead::Absent(_) => return Truth::False,
        InputRead::Withheld(_) => return Truth::Pending,
        InputRead::Failed(_) => return Truth::Failed,
    };
    let scalar = match scalar_value(applicable, path, &evidence) {
        ScalarEvidence::Known(scalar) => scalar,
        ScalarEvidence::Pending => return Truth::Pending,
        ScalarEvidence::Failed => return Truth::Failed,
    };
    match (&scalar, literal) {
        (InputLiteral::Int(integer), InputLiteral::Float(float))
        | (InputLiteral::Float(float), InputLiteral::Int(integer)) => {
            if crate::numeric::compare_int_float(*integer, *float).is_eq() {
                Truth::True
            } else {
                Truth::False
            }
        }
        (value, literal) if value == literal => Truth::True,
        (_, _) => Truth::False,
    }
}

fn condition_truth(applicable: &BoundInputRule<'_, '_>, condition: &InputCondition) -> Truth {
    match condition {
        InputCondition::Always => Truth::True,
        InputCondition::Present(path) => presence(path_evidence(applicable, path)),
        InputCondition::Equals { path, literal } => {
            scalar_equals(applicable, path, literal, path_evidence(applicable, path))
        }
        InputCondition::Not(inner) => match condition_truth(applicable, inner) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Pending => Truth::Pending,
            Truth::Failed => Truth::Failed,
        },
    }
}

fn failed_verdict(rule: &InputRule<Compiled>) -> InputVerdict {
    let mut violation = rule_violation(rule);
    violation.reason = Box::new(crate::schema::InputViolationReason::EvaluationFailed);
    InputVerdict::Violation(violation)
}

fn verdict(applicable: &BoundInputRule<'_, '_>) -> InputVerdict {
    let rule = applicable.rule;
    match condition_truth(applicable, rule.condition()) {
        Truth::False => return InputVerdict::Proven,
        Truth::Pending => return InputVerdict::Undecided,
        Truth::Failed => return failed_verdict(rule),
        Truth::True => {}
    }
    if applicable.pending_instance {
        return match path_evidence(applicable, rule.path()) {
            InputRead::Failed(_) => failed_verdict(rule),
            InputRead::Present(value) if value.failed() => failed_verdict(rule),
            _ => InputVerdict::Undecided,
        };
    }
    target_verdict(applicable)
}

fn target_verdict(applicable: &BoundInputRule<'_, '_>) -> InputVerdict {
    let rule = applicable.rule;
    let truth = match rule.predicate() {
        InputPredicate::Present => Some(presence(path_evidence(applicable, rule.path()))),
        InputPredicate::Absent => Some(match presence(path_evidence(applicable, rule.path())) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Pending => Truth::Pending,
            Truth::Failed => Truth::Failed,
        }),
        InputPredicate::Equals(literal) => Some(scalar_equals(
            applicable,
            rule.path(),
            literal,
            path_evidence(applicable, rule.path()),
        )),
        _ => None,
    };
    if let Some(truth) = truth {
        return match truth {
            Truth::True => InputVerdict::Proven,
            Truth::False => InputVerdict::Violation(rule_violation(rule)),
            Truth::Pending => InputVerdict::Undecided,
            Truth::Failed => failed_verdict(rule),
        };
    }
    // All owners consume the same evidence before using the same estimator.
    // Withheld inputs cannot acquire proof from an unapplied authored value.
    match path_evidence(applicable, rule.path()) {
        InputRead::Absent(proof) => {
            let optional = input_declaration(applicable, rule.path(), proof.depth() + 1)
                .is_some_and(|(_, mode)| mode == InputMode::Optional);
            if optional {
                InputVerdict::Proven
            } else {
                InputVerdict::Undecided
            }
        }
        InputRead::Withheld(value) => {
            if matches!(rule.target(), InputRuleTarget::MapKeys(_)) {
                value.target().map_or(InputVerdict::Undecided, |node| {
                    map_keys_verdict(applicable, &node)
                })
            } else {
                InputVerdict::Undecided
            }
        }
        InputRead::Failed(_) => failed_verdict(rule),
        InputRead::Present(value) => {
            if value.failed() {
                return failed_verdict(rule);
            }
            match rule.target() {
                InputRuleTarget::Value(_) => match scalar_value(applicable, rule.path(), &value) {
                    ScalarEvidence::Known(
                        InputLiteral::String(text)
                        | InputLiteral::Enum {
                            api_value: text, ..
                        },
                    ) => {
                        return evaluate_string(rule, &text)
                            .map_or_else(InputVerdict::Violation, |()| InputVerdict::Proven);
                    }
                    ScalarEvidence::Known(
                        InputLiteral::Bool(_) | InputLiteral::Int(_) | InputLiteral::Float(_),
                    )
                    | ScalarEvidence::Failed => return failed_verdict(rule),
                    ScalarEvidence::Pending => {}
                },
                InputRuleTarget::MapKeys(_) => return map_keys_verdict(applicable, &value),
            }
            value
                .expression()
                .map_or(InputVerdict::Undecided, |expression| {
                    estimator::estimate(applicable, rule_input::EstimatorInput::Value(expression))
                })
        }
    }
}

fn map_keys_verdict(applicable: &BoundInputRule<'_, '_>, evidence: &NodeRef<'_>) -> InputVerdict {
    let rule = applicable.rule;
    let cursor = evidence.keys();
    let mut pending = false;
    let defs = applicable.site.schema.as_schema().defs();
    let Some(ty) = literal_type(applicable, rule.path()) else {
        return failed_verdict(rule);
    };
    let crate::schema::Shape::Map { key, .. } = ty.shape_with_defs(defs) else {
        return failed_verdict(rule);
    };
    for (text, membership) in cursor.entries {
        let Ok(text) = crate::value::canonicalize_authored_map_key_text(text, key, defs) else {
            return failed_verdict(rule);
        };
        if let Err(violation) = evaluate_string(rule, &text) {
            match membership {
                Membership::Present => return InputVerdict::Violation(violation),
                Membership::Possible => pending = true,
            }
        }
    }
    let remainder = match cursor.remainder {
        rule_input::KeyRemainder::Complete => InputVerdict::Proven,
        rule_input::KeyRemainder::Withheld => InputVerdict::Undecided,
        rule_input::KeyRemainder::Expression(expression) => {
            estimator::estimate(applicable, rule_input::EstimatorInput::Keys(expression))
        }
    };
    match remainder {
        InputVerdict::Proven if pending => InputVerdict::Undecided,
        verdict => verdict,
    }
}

fn rule_violation(rule: &InputRule<Compiled>) -> InputViolation {
    InputViolation {
        path: rule.path().clone().into(),
        code: rule.code().clone(),
        reason: Box::new(rule.predicate().violation_reason()),
        condition: Box::new(rule.condition().clone()),
    }
}

fn peel_secret<P: ValueKind>(mut value: &Value<P>) -> &Value<P> {
    while let Value::Deferred(DeferredValue::Secret(secret)) = value {
        value = secret.expose_plaintext();
    }
    value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputFindingKind {
    Violation,
    CreateOnlyPreview,
}

/// Presentation severity derived from the finding kind in core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFindingSeverity {
    Error,
    Warning,
}

/// A value-free diagnostic shared by authored previews and finalized plans.
/// The classified rule carries its finding kind into the evaluator violation.
#[derive(Clone, PartialEq, Eq)]
pub struct InputFinding {
    resource_id: ResourceId,
    violation: InputViolation,
    kind: InputFindingKind,
    // Source metadata is ignored by semantic equality and never formatted:
    // authored keys may contain protected material.
    declaration: crate::parser::DeclarationProvenance,
}

impl std::fmt::Debug for InputFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputFinding")
            .field("resource_id", &self.resource_id)
            .field("violation", &self.violation)
            .field("kind", &self.kind)
            .finish()
    }
}

impl InputFinding {
    pub fn authored_location(&self) -> Option<&crate::parser::DeclarationLocation> {
        self.declaration
            .attribute_diagnostic_location(self.violation.path())
    }

    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }
    pub fn violation(&self) -> &InputViolation {
        &self.violation
    }
    pub fn severity(&self) -> InputFindingSeverity {
        match self.kind {
            InputFindingKind::Violation => InputFindingSeverity::Error,
            InputFindingKind::CreateOnlyPreview => InputFindingSeverity::Warning,
        }
    }
    pub fn message(&self) -> impl std::fmt::Display + '_ {
        struct Message<'a>(&'a InputFinding);
        impl std::fmt::Display for Message<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0.violation.message())?;
                match self.0.kind {
                    InputFindingKind::Violation => Ok(()),
                    InputFindingKind::CreateOnlyPreview => {
                        f.write_str(" (rejected if this resource is created)")
                    }
                }
            }
        }
        Message(self)
    }
}

impl std::fmt::Display for InputFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} [{}]: {}",
            self.resource_id,
            self.violation.path(),
            self.violation.code(),
            self.message()
        )
    }
}

/// A shared preview report retains unresolved obligations for callers that
/// want to distinguish an established pass from unavailable evidence.
#[derive(Debug, Clone, Default)]
pub struct AuthoredInputReport {
    findings: Vec<InputFinding>,
    pending: Vec<AuthoredPendingInput>,
    preparation_errors: Vec<AuthoredInputPreparationError>,
}

/// Preparation failed for this resource; other resources remain evaluable.
#[derive(Clone)]
pub struct AuthoredInputPreparationError {
    resource_id: ResourceId,
    error: crate::provider::HostError,
    declaration: crate::parser::DeclarationProvenance,
}

impl AuthoredInputPreparationError {
    pub fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }
    pub fn authored_location(&self) -> Option<&crate::parser::DeclarationLocation> {
        self.declaration.location()
    }
    pub fn into_host_error(self) -> crate::provider::HostError {
        self.error
    }
}
impl std::fmt::Display for AuthoredInputPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::fmt::Debug for AuthoredInputPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthoredInputPreparationError")
            .field("resource_id", &self.resource_id)
            .field("error", &self.error)
            .finish()
    }
}
impl std::error::Error for AuthoredInputPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Transparent metadata wrapper: retain the underlying cause chain
        // without repeating the same terminal error text as another link.
        std::error::Error::source(&self.error)
    }
}

/// Iteration includes every outcome, so clients cannot silently discard
/// resource preparation errors while rendering only successful evaluations.
pub enum AuthoredInputOutcome {
    Finding(InputFinding),
    Pending(AuthoredPendingInput),
    PreparationFailed(Box<AuthoredInputPreparationError>),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPendingInput {
    pub resource_id: ResourceId,
    pub path: InputDiagnosticPath,
    pub code: InputRuleCode,
}
impl AuthoredInputReport {
    /// Established rule findings only. Preparation errors and unresolved
    /// obligations remain explicit in the report's exhaustive outcome stream.
    pub fn findings(&self) -> &[InputFinding] {
        &self.findings
    }
    pub fn pending(&self) -> &[AuthoredPendingInput] {
        &self.pending
    }
    pub fn preparation_errors(&self) -> &[AuthoredInputPreparationError] {
        &self.preparation_errors
    }
    fn preparation_failed(
        &mut self,
        resource_id: &ResourceId,
        declaration: &crate::parser::DeclarationProvenance,
        error: crate::provider::HostError,
    ) {
        self.preparation_errors.push(AuthoredInputPreparationError {
            resource_id: resource_id.clone(),
            declaration: declaration.clone(),
            error,
        });
    }
}
impl IntoIterator for AuthoredInputReport {
    type Item = AuthoredInputOutcome;
    type IntoIter = std::vec::IntoIter<AuthoredInputOutcome>;
    fn into_iter(self) -> Self::IntoIter {
        let Self {
            findings,
            pending,
            preparation_errors,
        } = self;
        findings
            .into_iter()
            .map(AuthoredInputOutcome::Finding)
            .chain(pending.into_iter().map(AuthoredInputOutcome::Pending))
            .chain(
                preparation_errors
                    .into_iter()
                    .map(|error| AuthoredInputOutcome::PreparationFailed(Box::new(error))),
            )
            .collect::<Vec<_>>()
            .into_iter()
    }
}

/// Check authored inputs in a parser-owned configuration. Raw resource,
/// state and attribute-map callers cannot manufacture this source authority.
/// The shared classifier marks CreateOnly previews as warnings and EveryWrite
/// violations as errors. Unknown evidence remains a pending obligation.
/// A hand-built sparse file is not parser/admission evidence.
///
/// ```compile_fail,E0308
/// use carina_core::{input_rules::check_authored_inputs, parser::ExpandedParsedFile, resource::{Resource, Value, ConcreteValue}, schema::SchemaRegistry};
/// let mut patch_shaped = ExpandedParsedFile::default();
/// patch_shaped.resources.push(Resource::with_provider("mock", "Review", "example", None)
///     .with_attribute("target", Value::Concrete(ConcreteValue::String("changed".into()))));
/// let _ = check_authored_inputs(&patch_shaped, &SchemaRegistry::new());
/// ```
pub fn check_authored_inputs(
    parsed: &AuthoredInputs,
    schemas: &SchemaRegistry,
) -> AuthoredInputReport {
    let mut types = InputTypeContext::<ModuleSource>::empty();
    types.facts.defaults = parsed.defaults().to_vec();
    types.facts.selection = parsed.selection().clone();
    let mut report = AuthoredInputReport::default();
    let declarations = Arc::new(types.facts);
    for source in parsed.resources() {
        let resource = source.resource();
        let Some(schema) = schemas.get_for(resource) else {
            continue;
        };
        let input =
            match RuleInput::for_authored(&source, schema.schema(), Arc::clone(&declarations)) {
                Ok(input) => input,
                Err(error) => {
                    report.preparation_failed(&resource.id, &resource.declaration, error);
                    continue;
                }
            };
        let site = match classify_write_site::<ModuleSource>(
            WriteSiteSource::Preview {
                resource_id: resource.id.clone(),
                input,
            },
            schemas,
        ) {
            Ok(site) => site,
            Err(error) => {
                report.preparation_failed(
                    &resource.id,
                    &resource.declaration,
                    crate::provider::HostError::invalid_input_caused_by(
                        "input rule preparation failed",
                        error,
                    ),
                );
                continue;
            }
        };
        if let Some(site) = site {
            for outcome in site.evaluate().into_outcomes() {
                match outcome {
                    InputEvaluationOutcome::Finding(finding) => report.findings.push(*finding),
                    InputEvaluationOutcome::Pending { path, code } => {
                        report.pending.push(AuthoredPendingInput {
                            resource_id: resource.id.clone(),
                            path,
                            code,
                        })
                    }
                }
            }
        }
    }
    report
}

fn project_attributes<P: crate::resource::PlaceholderKind, B: crate::resource::BindingAddress>(
    attributes: &IndexMap<String, Value<P, B>>,
) -> InputAttributes {
    attributes
        .iter()
        .map(|(key, value)| (key.clone(), project_known_value(value)))
        .collect()
}

/// Preserve shared knowledge, container membership and protected failures while
/// retaining typed parser/template substitutions. Read-back values never enter this view.
fn project_known_value<P: crate::resource::PlaceholderKind, B: crate::resource::BindingAddress>(
    value: &Value<P, B>,
) -> InputValue {
    value.input_preview()
}

/// Every desired write slot either passed or retains an explicit obligation.
/// Cloning this immutable owner preserves the same effects; mutable/raw plans
/// cannot recover its execution authority.
///
/// ```compile_fail,E0277
/// use carina_core::{input_rules::InputCheckedPlan, plan::Plan};
/// let checked: InputCheckedPlan = Plan::new().into();
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::InputCheckedPlan, resource::Resource};
/// let checked: InputCheckedPlan = Resource::new("test", "r").into();
/// ```
/// ```compile_fail,E0277
/// use carina_core::{input_rules::InputCheckedPlan, effect::Effect};
/// fn forge(effect: Effect) -> InputCheckedPlan { effect.into() }
/// ```
/// ```compile_fail,E0451
/// use carina_core::{input_rules::InputCheckedPlan, plan::AdmittedPlan};
/// fn forge(plan: AdmittedPlan) -> InputCheckedPlan {
///     InputCheckedPlan { plan, obligations: todo!() }
/// }
/// ```
/// ```compile_fail,E0277
/// use carina_core::input_rules::InputCheckedPlan;
/// let checked: InputCheckedPlan = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0596
/// use carina_core::{input_rules::InputCheckedPlan, effect::Effect};
/// fn mutate(checked: &mut InputCheckedPlan, effect: Effect) { checked.add(effect); }
/// ```
/// ```compile_fail,E0599
/// use carina_core::input_rules::InputCheckedPlan;
/// let checked = InputCheckedPlan::default();
/// ```
#[derive(Debug, Clone)]
pub struct InputCheckedPlan<S = ModuleSource> {
    plan: crate::plan::AdmittedPlan<S>,
    obligations: Box<InputObligations>,
}

#[derive(Debug, Clone)]
struct InputObligations {
    pending: Vec<PendingInputObligation>,
    write_checks: Vec<PendingInputObligation>,
}

impl<S> Deref for InputCheckedPlan<S> {
    type Target = crate::plan::AdmittedPlan<S>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl<S> InputCheckedPlan<S> {
    pub(crate) fn execution_input_checks(&self) -> Vec<PendingInputObligation> {
        self.obligations.write_checks.clone()
    }
    pub fn plan(&self) -> &crate::plan::AdmittedPlan<S> {
        &self.plan
    }
    pub fn pending_inputs(&self) -> &[PendingInputObligation] {
        &self.obligations.pending
    }
    pub(crate) fn pending_rows(&self, effect_index: usize) -> Vec<crate::detail_rows::DetailRow> {
        let mut paths: BTreeMap<InputDiagnosticPath, Vec<InputRuleCode>> = BTreeMap::new();
        for obligation in &self.obligations.pending {
            if obligation.target.effect_index == effect_index {
                for (path, code) in &obligation.pending {
                    paths.entry(path.clone()).or_default().push(code.clone());
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

#[cfg(test)]
impl<S: Clone> crate::plan::AdmittedPlan<S> {
    /// Check the admitted, prepared effects and retain both witnesses for execution.
    pub(crate) fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>> {
        self.check_inputs_with_types(schemas, current, &InputTypeContext::empty())
    }

    /// Finalize the admitted effects and check them against retained declarations.
    pub(crate) fn check_inputs_with_types(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
        types: &InputTypeContext<S>,
    ) -> Result<InputCheckedPlan<S>, InputAdmissionFailure<S>> {
        FinalizedPlan::new(self.into_unadmitted()).check_inputs_with_types(schemas, current, types)
    }
}

/// Only executor discharge can supply this unforgeable constructor token.
///
/// ```compile_fail,E0603
/// use carina_core::input_rules::executor_write_site;
/// ```
pub(crate) fn executor_write_site<'a>(
    authority: &'a crate::executor::input_discharge::DischargeToken,
    admission: &'a PendingInputObligation,
    input: Arc<RuleInput>,
    schemas: &'a SchemaRegistry,
) -> Result<Option<WriteSite<'a>>, crate::provider::UpdatePatchError> {
    classify_write_site::<ModuleSource>(
        WriteSiteSource::Executor {
            authority,
            admission,
            input,
        },
        schemas,
    )
}

fn evaluate_string(rule: &InputRule<Compiled>, value: &str) -> Result<(), InputViolation> {
    let accepted = match rule.predicate() {
        InputPredicate::Present | InputPredicate::Absent | InputPredicate::Equals(_) => {
            unreachable!("general predicates are evaluated before string predicates")
        }
        InputPredicate::Length { min, max } => {
            let count = u64::try_from(value.chars().count()).expect("string length fits u64");
            min.is_none_or(|min| count >= min) && max.is_none_or(|max| count <= max)
        }
        InputPredicate::AllowedChars(set) => value.chars().all(|ch| set.contains(ch)),
        InputPredicate::FirstChar(set) => value.chars().next().is_some_and(|ch| set.contains(ch)),
        InputPredicate::LastChar(set) => {
            value.chars().next_back().is_some_and(|ch| set.contains(ch))
        }
        InputPredicate::NoSubstring(token) => !value.contains(token.as_str()),
        InputPredicate::NoPrefix(token) => !value.starts_with(token.as_str()),
        InputPredicate::ForbiddenPrefix {
            prefix,
            ignore_ascii_case,
        } => !value.get(..prefix.as_str().len()).is_some_and(|head| {
            if *ignore_ascii_case {
                head.eq_ignore_ascii_case(prefix.as_str())
            } else {
                head == prefix.as_str()
            }
        }),
        InputPredicate::NoSuffix(token) => !value.ends_with(token.as_str()),
        InputPredicate::NoAdjacent { left, right } => !value
            .chars()
            .zip(value.chars().skip(1))
            .any(|(a, b)| left.contains(a) && right.contains(b)),
        InputPredicate::NotIpv4 => !is_ipv4(value),
        InputPredicate::Matches(pattern) => pattern.is_match(value),
        InputPredicate::NotMatches(pattern) => !pattern.is_match(value),
    };
    if accepted {
        Ok(())
    } else {
        Err(rule_violation(rule))
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

#[cfg(test)]
fn authored_fixture_with_types<'a, S, P: crate::resource::PlaceholderKind>(
    resource: &crate::resource::Resource<ModuleSource, P>,
    schemas: &'a SchemaRegistry,
    types: &InputTypeContext<S>,
) -> WriteSite<'a> {
    let schema = schemas.get_for(resource).expect("fixture schema").schema();
    let authored = AuthoredInputs::resource_for_test(resource);
    let source = authored.resources().next().unwrap();
    let input = RuleInput::for_authored(&source, schema, Arc::new(types.facts.clone()))
        .expect("fixture authored input");
    classify_write_site::<ModuleSource>(
        WriteSiteSource::Preview {
            resource_id: resource.id.clone(),
            input,
        },
        schemas,
    )
    .unwrap()
    .unwrap()
}
