//! Runtime proofs for evaluation and collection shape.
//!
//! Classification grants neither transport permission nor access to Secret data.
//!
//! Proof construction is private, including for concrete collections.
//!
//! ```compile_fail,E0277
//! use carina_core::resource::{Value, knowledge::KnownValue};
//! fn forge(raw: Value) { let _: KnownValue = raw.into(); }
//! ```
//! ```compile_fail,E0616
//! use carina_core::resource::knowledge::KnownValue;
//! fn forge(value: KnownValue) { let _ = value.node; }
//! ```
//! ```compile_fail,E0599
//! use carina_core::resource::{NoPlaceholder, AbsoluteBindingAddress, knowledge::ProtectedSecret};
//! fn leak(secret: &ProtectedSecret<'_, NoPlaceholder, AbsoluteBindingAddress>) -> String {
//!     secret.expose_plaintext().to_owned()
//! }
//! ```
//! Classification's internal visitors are unavailable to general value walkers.
//!
//! ```compile_fail,E0624
//! use carina_core::resource::knowledge::KnownValue;
//! fn outside_walker(value: &KnownValue) { value.visit(|_| (), |_| ()); }
//! ```
//!
//! Pending inspection cannot export plaintext paths or text.
//!
//! ```compile_fail,E0277
//! use carina_core::resource::knowledge::PendingSite;
//! fn leak(site: &PendingSite) {
//!     let _ = site.inspect::<String, std::convert::Infallible>(|path, _, _| Ok(path.to_string()));
//! }
//! ```
//! ```compile_fail,E0277
//! use carina_core::{resource::knowledge::PendingSite, schema::LeafPath};
//! fn leak(site: &PendingSite) {
//!     let _ = site.inspect::<LeafPath, std::convert::Infallible>(|path, _, _| Ok(path.clone()));
//! }
//! ```
//! ```compile_fail,E0277
//! use carina_core::resource::knowledge::PendingSite;
//! fn leak(site: &PendingSite) {
//!     let _ = site.inspect::<serde_json::Value, std::convert::Infallible>(|path, _, _| Ok(serde_json::json!(path.to_string())));
//! }
//! ```
//! Historical hashes cannot enter this live classifier.
//!
//! ```compile_fail,E0599
//! use carina_core::resource::{SnapshotValue, knowledge::KnowledgeContext};
//! fn historical(value: &SnapshotValue) { let _ = value.knowledge(&KnowledgeContext::validation()).map(|proof| proof.into_classification()); }
//! ```

//! Sites remain runtime-only, including when copied into obligations.
//!
//! ```compile_fail,E0277
//! use carina_core::resource::knowledge::PendingSite;
//! fn persist(site:&PendingSite){let _=serde_json::to_value(site);}
//! ```
//! ```compile_fail,E0599
//! use carina_core::resource::knowledge::ApplyPending;
//! fn plan_phase(value:&ApplyPending){let _=value.phase();}
//! ```

use super::{
    AbsoluteBindingAddress, AccessPath, ApplyTimeUnknown, BindingAddress, CanonicalEnumValue,
    ConcreteValue, DeferredValue, DiagnosticScope, InsideSecret, InterpolationPart, NoPlaceholder,
    ParserPlaceholder, PlaceholderKind, RawEnumIdentifier, SecretDiagnosticError, SecretPayload,
    SecretReadResult, Value, ValueString,
};
use crate::resolver::ResolutionError;
use crate::{binding_index::ResolvedBindings, schema::LeafPath};
use carina_provider_protocol::FiniteF64;
use indexmap::IndexMap;
use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

#[cfg(test)]
mod decision_contract {
    use super::*;
    use crate::schema::LeafPathSegment;
    use std::convert::Infallible;

    fn failed() -> Value {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: Vec::new(),
            reason: super::super::UnavailableReason::from_builtin_error("already attempted".into()),
            source_line: 0.into(),
        })
    }

    #[test]
    fn attempted_failure_is_structural_evidence_and_never_a_pending_value() {
        let value = failed();
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(proof, Classification::Failed(_)));
        assert_eq!(proof.sites().count(), 0);
        assert!(matches!(
            PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(KnowledgeError::Unavailable { .. })
        ));

        let value: Value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(7)),
            failed(),
        ]));
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        let Classification::ShapeKnown(shape) = &proof else {
            panic!("membership remains available")
        };
        assert_eq!(shape.len(), 2);
        shape.visit(
            |view| match view {
                ShapeView::List(items) => {
                    assert!(matches!(items[0], Classification::Known(_)));
                    assert!(matches!(items[1], Classification::Failed(_)));
                }
                ShapeView::Map(_) => panic!("list"),
            },
            |_| panic!("ordinary list"),
        );
        assert!(matches!(
            PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(KnowledgeError::Unavailable { .. })
        ));
    }

    #[test]
    fn value_projection_rejects_failed_descendants_and_static_scheduling_requires_evidence() {
        let value: Value = Value::Concrete(ConcreteValue::List(vec![failed()]));
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(
            proof.clone().into_known(),
            Err(KnowledgeError::Unavailable { .. })
        ));
        assert!(!proof.permits_static_evaluation());
        let protected: Value = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::root_resource_ref("producer", "identifier", vec![])).into(),
        ));
        let proof = protected
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(proof, Classification::Pending(_)));
        assert!(proof.sites().next().is_some());
        assert!(
            proof
                .sites()
                .all(|site| site.scope == DiagnosticScope::Secret)
        );
        assert!(!proof.permits_static_evaluation());
    }

    #[test]
    fn classification_keeps_secret_payloads_opaque_even_after_attempted_failure() {
        for (inner, attempted) in [
            (failed(), true),
            (
                Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
                    super::super::PendingReRead::not_found(),
                ))),
                false,
            ),
        ] {
            let value: Value = Value::Deferred(DeferredValue::Secret(Box::new(inner).into()));
            let proof = value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap();
            match &proof {
                Classification::Pending(pending) => {
                    assert!(!attempted);
                    pending.visit(
                        |_, _, _| panic!("ordinary callback must not see Secret"),
                        |_| (),
                    );
                    assert!(pending.sites().next().is_some());
                    assert!(
                        pending
                            .sites()
                            .all(|site| site.scope == DiagnosticScope::Secret)
                    );
                }
                Classification::Failed(failed) => {
                    assert!(attempted);
                    assert!(ClassifiedRef::from(&proof).secret().is_some());
                    assert!(matches!(
                        failed.error(),
                        KnowledgeError::Unavailable {
                            reason: SecretDiagnosticError::InsideSecret,
                            ..
                        }
                    ));
                    assert!(!failed.error().to_string().contains("already attempted"));
                    assert_eq!(proof.sites().count(), 0);
                }
                Classification::Known(_) | Classification::ShapeKnown(_) => {
                    panic!("Secret is an opaque pending or failed node")
                }
            }
            assert!(matches!(
                proof.to_value(),
                Value::Deferred(DeferredValue::Secret(_))
            ));
            assert!(!format!("{proof:?}").contains("already attempted"));
        }
    }

    #[test]
    fn guarded_conditional_failure_is_retained_but_inactive() {
        use crate::parser::{DeferredIfCondition, DeferredValueIf};
        use crate::resource::RelativeBindingName;
        let condition = DeferredIfCondition::new(
            Value::Concrete(ConcreteValue::Bool(true)),
            "conditional.crn".into(),
            1,
        );
        let failure: Value<ParserPlaceholder, RelativeBindingName> =
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: vec![],
                reason: super::super::UnavailableReason::from_builtin_error("unselected".into()),
                source_line: 1.into(),
            });
        let value = Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
            condition,
            Value::Concrete(ConcreteValue::String("kept".into())),
            failure,
        ))));
        let bindings = ResolvedBindings::default();
        let proof = value
            .knowledge(&KnowledgeContext::plan(&bindings))
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(
            proof.failure().is_none(),
            "unselected branch is guarded evidence"
        );
        assert!(
            matches!(
                PlanResolved::try_from(
                    value
                        .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                            &crate::binding_index::ResolvedBindings::default()
                        ))
                        .unwrap()
                ),
                Err(KnowledgeError::UnresolvedAtBoundary { .. })
            ),
            "the pending discriminator remains a Plan operation, not an active failure"
        );
    }

    #[test]
    fn failed_required_attribute_stays_present_and_keeps_its_sibling() {
        let required: Value = Value::root_resource_ref("cycle", "value", vec![]);
        let sibling: Value = Value::Concrete(ConcreteValue::Int(7));
        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "cycle",
            HashMap::from([("value".into(), required.clone())]),
            crate::binding_index::BindingValueSource::Local,
        );
        let mut errors = Vec::new();
        let classified = collect_classified_attributes(
            [
                ("required".to_owned(), &required),
                ("sibling".to_owned(), &sibling),
            ],
            &KnowledgeContext::plan(&bindings),
            |name, error| errors.push((name.to_owned(), error)),
        );
        assert_eq!(classified.len(), 2);
        assert!(matches!(classified["required"], Classification::Failed(_)));
        assert!(matches!(classified["sibling"], Classification::Known(_)));
        assert!(
            matches!(errors.as_slice(), [(name, KnowledgeError::Cycle { .. })] if name == "required")
        );
    }

    #[test]
    fn apply_discharge_consumes_apply_stage_evidence() {
        let value: Value = Value::Concrete(ConcreteValue::Int(7));
        let bindings = ResolvedBindings::default();
        let apply = value
            .knowledge(&KnowledgeContext::apply(&bindings))
            .unwrap();
        assert_eq!(KnownValue::from_apply(apply).unwrap().to_value(), value);
    }

    #[test]
    fn reference_cycles_use_selector_identity_not_redacted_display() {
        use crate::binding_index::{BindingCompleteness, BindingValueSource};
        use crate::resource::{RedactedSecretHash, SnapshotValue, Subscript};
        let path = |key| {
            AccessPath::root_with_fields_and_subscripts(
                "producer",
                "items",
                vec![],
                vec![Subscript::Str {
                    key: ValueString::diagnostic_sensitive(key),
                }],
            )
        };
        assert_eq!(path("first").to_string(), path("second").to_string());
        assert_ne!(path("first"), path("second"));
        let alias: Value = Value::Deferred(DeferredValue::ResourceRef {
            path: path("second"),
        });
        let digest = RedactedSecretHash::from_encoded_digest(&format!(
            "argon2-legacy:73616c74:{}",
            "ab".repeat(32)
        ))
        .unwrap();
        // A historical hash forces selection before conversion. The first key
        // refers to the second; neither key is a cycle, despite equal display.
        let snapshot: SnapshotValue = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("first".into(), alias.to_snapshot()),
            (
                "second".into(),
                Value::Concrete(ConcreteValue::Int(7)).to_snapshot(),
            ),
            ("unrelated_hash".into(), SnapshotValue::redacted(digest)),
        ])));
        let mut bindings = ResolvedBindings::default();
        bindings.set_historical_with_completeness(
            "producer",
            HashMap::from([("items".into(), snapshot)]),
            BindingValueSource::Local,
            BindingCompleteness::Complete,
        );
        let value: Value = Value::Deferred(DeferredValue::ResourceRef {
            path: path("first"),
        });
        let proof = value.knowledge(&KnowledgeContext::plan(&bindings));
        assert!(
            matches!(proof, Ok(ref proof) if proof.sites().all(|site| site.phase() == Phase::Plan)),
            "{proof:?}"
        );
    }

    #[test]
    fn sites_enumerate_leaf_paths_without_losing_map_key_identity() {
        let value: Value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "outer.key".into(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([(
                    "inner.key".into(),
                    Value::root_resource_ref("producer", "identifier", Vec::new()),
                )])),
            )])),
        )])));
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        for _ in 0..2 {
            let sites = proof.sites().collect::<Vec<_>>();
            assert_eq!(sites.len(), 1);
            assert!(
                sites[0]
                    .inspect::<bool, Infallible>(|path, operands, _| Ok(path.segments()
                        == [
                            LeafPathSegment::MapKey("outer.key".into()),
                            LeafPathSegment::ListIndex(0),
                            LeafPathSegment::MapKey("inner.key".into()),
                        ]
                        && operands.is_empty()))
                    .unwrap()
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Plan,
    Apply,
}
/// Requirements are fixed by semantic operations, never relaxation flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    ShapeForPlan,
}
#[derive(Clone)]
pub struct PhaseViolation<B: BindingAddress = AbsoluteBindingAddress> {
    required: Requirement,
    site: Box<PendingSite<B>>,
    source: Option<crate::parser::DeclarationProvenance>,
}
impl<B: BindingAddress> PhaseViolation<B> {
    pub fn required(&self) -> Requirement {
        self.required
    }
    pub fn phase(&self) -> Phase {
        self.site.phase()
    }
    pub fn reason(&self) -> &PendingReason {
        self.site.reason()
    }
    pub fn source(&self) -> Option<&crate::parser::DeclarationProvenance> {
        self.source.as_ref()
    }
    pub fn with_source(mut self, source: crate::parser::DeclarationProvenance) -> Self {
        self.source = Some(source);
        self
    }
    pub fn inspect<R: SecretReadResult, E: InsideSecret>(
        &self,
        f: impl FnOnce(&LeafPath, &[OperandStep], &WaitOn<B>) -> Result<R, E>,
    ) -> Result<R, E> {
        self.site.inspect(f)
    }
}
impl<B: BindingAddress> InsideSecret for PhaseViolation<B> {
    fn inside_secret(mut self) -> Self {
        self.site.scope = DiagnosticScope::Secret;
        self
    }
}
impl<B: BindingAddress> fmt::Debug for PhaseViolation<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhaseViolation")
            .field("required", &self.required)
            .field("site", &self.site)
            .finish()
    }
}
impl<B: BindingAddress> fmt::Display for PhaseViolation<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "requires {:?}, available at {:?}",
            self.required,
            self.phase()
        )?;
        match self.site.scope {
            DiagnosticScope::Secret => f.write_str(" (protected value)"),
            DiagnosticScope::Public => {
                if !self.site.path.segments().is_empty() {
                    write!(f, " at {}", self.site.path)?;
                }
                match &self.site.wait_on {
                    WaitOn::Resource(path) => write!(f, "; waiting for {path}"),
                    WaitOn::Binding(binding) => write!(f, "; waiting for {binding}"),
                    WaitOn::Unknown(_) => f.write_str("; waiting for a published value"),
                    WaitOn::Evaluation(kind) => write!(f, "; waiting for {kind:?} evaluation"),
                    WaitOn::Substitution(kind) => write!(f, "; waiting for {kind:?} substitution"),
                }
            }
        }
    }
}
impl<B: BindingAddress> std::error::Error for PhaseViolation<B> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationKind {
    Coalesce,
    IntegerArithmetic,
    Interpolation,
    Builtin,
    UserFunction,
    Access,
    ModuleIf,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstitutionKind {
    ModuleArgument,
    FnParam,
    FnLocal,
    LoopKey,
    LoopIndex,
    LoopValue,
    EmptyInterpolation,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingReason {
    ResourceSelection,
    BindingSelection,
    UpstreamExport,
    UpstreamAllExports,
    ReadBack,
    ModuleArgument,
    FunctionParameter,
    FunctionLocal,
    LoopKey,
    LoopIndex,
    LoopValue,
    IncompleteExpression,
    Evaluation(EvaluationKind),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperandStep {
    Argument(usize),
    ArithmeticLeft,
    ArithmeticRight,
    Interpolation(usize),
    AccessBase,
    CoalesceLeft,
    CoalesceFallback,
    Condition,
    Then,
    Else,
    FunctionBody,
    FunctionLocal(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOn<B: BindingAddress = AbsoluteBindingAddress> {
    Resource(AccessPath<B>),
    Binding(B),
    Unknown(ApplyTimeUnknown<B>),
    Substitution(SubstitutionKind),
    Evaluation(EvaluationKind),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingGuard {
    Required,
    CoalesceFallback,
    ConditionalBranch,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PendingSite<B: BindingAddress = AbsoluteBindingAddress> {
    path: LeafPath,
    operand: Vec<OperandStep>,
    wait_on: WaitOn<B>,
    phase: Phase,
    reason: PendingReason,
    scope: DiagnosticScope,
    guard: PendingGuard,
}
impl<B: BindingAddress> PendingSite<B> {
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn reason(&self) -> &PendingReason {
        &self.reason
    }
    pub fn inspect<R: SecretReadResult, E: InsideSecret>(
        &self,
        f: impl FnOnce(&LeafPath, &[OperandStep], &WaitOn<B>) -> Result<R, E>,
    ) -> Result<R, E> {
        f(&self.path, &self.operand, &self.wait_on).map_err(|error| self.scope.protect_error(error))
    }
}
impl<B: BindingAddress> fmt::Debug for PendingSite<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSite")
            .field("phase", &self.phase)
            .field("reason", &self.reason)
            .field("scope", &self.scope)
            .field("guard", &self.guard)
            .finish_non_exhaustive()
    }
}

/// Collect each authored attribute independently. A failed classification stays
/// present as typed failure evidence so required-attribute checks cannot invent
/// an additional missing-input diagnostic. Classifier errors are reported once;
/// existing Failed nodes retain the structural validation policy from main.
/// Callers cannot pair an arbitrary source value with fabricated failure evidence.
///
/// ```compile_fail,E0271
/// use carina_core::resource::{ConcreteValue, Value, NoPlaceholder, AbsoluteBindingAddress};
/// use carina_core::resource::knowledge::{collect_classified_attributes, Classification, KnowledgeContext, KnowledgeError};
/// let source: Value = Value::Concrete(ConcreteValue::Int(7));
/// let fabricated: Result<Classification<'_>, KnowledgeError> =
///     Err(KnowledgeError::InvalidStage { operation: "forged" });
/// let _ = collect_classified_attributes(
///     [("input".to_owned(), &source, fabricated)],
///     &KnowledgeContext::<NoPlaceholder, AbsoluteBindingAddress>::validation(),
///     |_, _| {},
/// );
/// ```
pub fn collect_classified_attributes<
    'a,
    P: PlaceholderKind,
    B: BindingAddress,
    S: ClassificationStage,
>(
    attributes: impl IntoIterator<Item = (String, &'a Value<P, B>)>,
    context: &KnowledgeContext<'_, P, B, S>,
    mut failed: impl FnMut(&str, KnowledgeError<B>),
) -> HashMap<String, Classification<'a, P, B>> {
    attributes
        .into_iter()
        .map(|(name, source)| {
            let classification = match source.knowledge(context) {
                Ok(knowledge) => knowledge.into_classification(),
                Err(error) => {
                    failed(&name, error.clone());
                    Classification::Failed(AttemptedFailure { source, error })
                }
            };
            (name, classification)
        })
        .collect()
}

mod stage_sealed {
    pub trait Sealed {}
}
/// The marker selected by the classification context. Plan and Apply contexts
/// both accept the same `ResolvedBindings` type.
pub trait ClassificationStage: stage_sealed::Sealed {}
#[derive(Clone, Copy, Debug)]
pub enum Validation {}
#[derive(Clone, Copy, Debug)]
pub enum Plan {}
#[derive(Clone, Copy, Debug)]
pub enum Apply {}
impl stage_sealed::Sealed for Validation {}
impl stage_sealed::Sealed for Plan {}
impl stage_sealed::Sealed for Apply {}
impl ClassificationStage for Validation {}
impl ClassificationStage for Plan {}
impl ClassificationStage for Apply {}

/// A classification retains the stage that supplied its binding facts.
/// Structural consumers may inspect its classified tree; doing so cannot mint
/// a plan-stage classification from validation or apply evidence.
#[derive(Clone)]
pub struct Knowledge<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    S: ClassificationStage = Validation,
> {
    classification: Classification<'a, P, B>,
    stage: std::marker::PhantomData<S>,
}
impl<'a, P: PlaceholderKind, B: BindingAddress, S: ClassificationStage> Knowledge<'a, P, B, S> {
    pub fn into_classification(self) -> Classification<'a, P, B> {
        self.classification
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress, S: ClassificationStage> std::ops::Deref
    for Knowledge<'a, P, B, S>
{
    type Target = Classification<'a, P, B>;
    fn deref(&self) -> &Self::Target {
        &self.classification
    }
}
impl<P: PlaceholderKind, B: BindingAddress, S: ClassificationStage> fmt::Debug
    for Knowledge<'_, P, B, S>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.classification.fmt(f)
    }
}

/// Structural classification without a stage marker. A fully known result can
/// yield a stage-independent `KnownValue` through `into_known`.
#[derive(Clone)]
pub enum Classification<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    T = Pending<'a, P, B>,
> {
    Known(KnownValue<'a, P, B>),
    ShapeKnown(ShapeKnown<'a, P, B, T>),
    Pending(T),
    Failed(AttemptedFailure<'a, P, B>),
}

/// One flatten element with an admitted inner shape. This builtin projection
/// keeps pending protected elements opaque and retains their call.
pub(crate) enum FlattenItem<'s, 'a, P: PlaceholderKind, B: BindingAddress> {
    Known(&'s KnownValue<'a, P, B>),
    Shape(&'s ShapeKnown<'a, P, B>),
}
impl<P: PlaceholderKind, B: BindingAddress> FlattenItem<'_, '_, P, B> {
    pub(crate) fn append_to(self, output: &mut Vec<Value<P, B>>) {
        match self {
            Self::Known(value) => output.extend(value.visit(
                |view| match view {
                    KnownView::List(items) => {
                        items.iter().map(KnownValue::to_value).collect::<Vec<_>>()
                    }
                    _ => vec![value.to_value()],
                },
                |_| vec![value.to_value()],
            )),
            Self::Shape(value) => output.extend(value.visit(
                |view| {
                    match view {
                        ShapeView::List(items) => items
                            .iter()
                            .map(Classification::to_value)
                            .collect::<Vec<_>>(),
                        ShapeView::Map(_) => vec![value.to_value()],
                    }
                },
                |_| vec![value.to_value()],
            )),
        }
    }
}

impl<'a, P: PlaceholderKind, B: BindingAddress> Classification<'a, P, B> {
    pub(crate) fn flatten_item(
        &self,
    ) -> Result<Option<FlattenItem<'_, 'a, P, B>>, KnowledgeError<B>> {
        match self {
            Self::Known(value) => Ok(Some(FlattenItem::Known(value))),
            Self::ShapeKnown(value) => Ok(Some(FlattenItem::Shape(value))),
            Self::Pending(_) => Ok(None),
            Self::Failed(value) => Err(value.error()),
        }
    }
}
/// A fully known, stage-independent value. Structural `Classification::into_known`
/// can produce this proof; `from_apply` additionally checks its input stage.
#[derive(Clone)]
pub struct KnownValue<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    node: kernel::KnownNode<'a, P, B>,
    source: &'a Value<P, B>,
}
#[derive(Clone)]
pub struct KnownList<'a, P: PlaceholderKind, B: BindingAddress> {
    items: Vec<KnownValue<'a, P, B>>,
}
#[derive(Clone)]
pub struct KnownMap<'a, P: PlaceholderKind, B: BindingAddress> {
    entries: IndexMap<&'a str, KnownValue<'a, P, B>>,
}
pub(crate) enum KnownView<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    String(&'a ValueString),
    EnumIdentifier(&'a RawEnumIdentifier),
    CanonicalEnum(&'a CanonicalEnumValue),
    Int(i64),
    Float(FiniteF64),
    Bool(bool),
    Duration(Duration),
    List(&'a KnownList<'a, P, B>),
    StringList(&'a [ValueString]),
    Map(&'a KnownMap<'a, P, B>),
}
#[derive(Clone)]
pub struct ProtectedSecret<'a, P: PlaceholderKind, B: BindingAddress> {
    payload: &'a SecretPayload<P, B>,
}

// Only this private wrapper can return a protected classification. Its inner
// proof is never delivered to ordinary visitors or exported outside Classification.
struct SecretClassification<'a, P: PlaceholderKind, B: BindingAddress>(Classification<'a, P, B>);
impl<P: PlaceholderKind, B: BindingAddress> super::SealedSecretReadResult
    for SecretClassification<'_, P, B>
{
}
impl<P: PlaceholderKind, B: BindingAddress> SecretReadResult for SecretClassification<'_, P, B> {}

#[derive(Clone)]
pub struct ShapeKnown<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    T = Pending<'a, P, B>,
> {
    source: &'a Value<P, B>,
    node: kernel::ShapeNode<'a, P, B, T>,
    apply_required: bool,
}
pub(crate) enum ShapeView<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    T = Pending<'a, P, B>,
> {
    List(&'a [Classification<'a, P, B, T>]),
    Map(&'a IndexMap<&'a str, Classification<'a, P, B, T>>),
}
/// Attempted evaluation failure is evidence, never a wait or a known value.
#[derive(Clone)]
pub struct AttemptedFailure<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    source: &'a Value<P, B>,
    error: KnowledgeError<B>,
}
impl<P: PlaceholderKind, B: BindingAddress> AttemptedFailure<'_, P, B> {
    pub fn error(&self) -> KnowledgeError<B> {
        self.error.clone()
    }
}
#[derive(Clone)]
pub struct Pending<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    source: &'a Value<P, B>,
    node: kernel::PendingNode<'a, P, B, Self, kernel::Input<'a, P, B>>,
    phase: Phase,
    reason: PendingReason,
    wait_on: WaitOn<B>,
}
#[derive(Clone)]
pub struct PlanIterable<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    T = Pending<'a, P, B>,
> {
    node: kernel::IterableNode<'a, P, B, T>,
}
/// A pending result admitted at the plan boundary. Its constructor and sites
/// are private and there is no phase field to set.
#[derive(Clone)]
pub struct ApplyPending<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    source: &'a Value<P, B>,
    reason: PendingReason,
    wait_on: WaitOn<B>,
    node: kernel::PendingNode<'a, P, B, Self, kernel::ApplyInput<'a, P, B>>,
}
/// A validation classification cannot cross the plan boundary.
///
/// ```compile_fail,E0277
/// use carina_core::resource::{Value, ConcreteValue};
/// use carina_core::resource::knowledge::{KnowledgeContext, PlanResolved};
/// let value: Value = Value::Concrete(ConcreteValue::Int(1));
/// let validation = value.knowledge(&KnowledgeContext::validation()).unwrap();
/// let _ = PlanResolved::try_from(validation);
/// ```
/// ```compile_fail,E0277
/// use carina_core::{binding_index::ResolvedBindings, resource::{Value, ConcreteValue}};
/// use carina_core::resource::knowledge::{KnowledgeContext, PlanResolved};
/// let value: Value = Value::Concrete(ConcreteValue::Int(1));
/// let bindings = ResolvedBindings::default();
/// let apply = value.knowledge(&KnowledgeContext::apply(&bindings)).unwrap();
/// let _ = PlanResolved::try_from(apply);
/// ```
/// The recursively projected result of plan-time resolution.
/// Active failures are rejected. Unselected coalesce alternatives retain their
/// classified failure evidence behind the pending node until selection.
///
/// ```compile_fail,E0308
/// use carina_core::resource::knowledge::{Classification, PlanResolved};
/// fn bypass(raw: Classification) { let _: PlanResolved = raw; }
/// ```
#[derive(Clone)]
pub struct PlanResolved<
    'a,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
> {
    knowledge: Classification<'a, P, B, ApplyPending<'a, P, B>>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnowledgeError<B: BindingAddress = AbsoluteBindingAddress> {
    Unavailable {
        name: String,
        reason: SecretDiagnosticError<super::UnavailableReason>,
        source_line: super::DiagnosticSourceLine,
    },
    InvalidStage {
        operation: &'static str,
    },
    Cycle {
        operation: &'static str,
    },
    UnavailableHistoricalSecret(super::UnavailableSecretError),
    UnresolvedAtBoundary {
        sites: Vec<PendingSite<B>>,
    },
}
impl<B: BindingAddress> fmt::Display for KnowledgeError<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                name,
                reason,
                source_line: _,
            } => write!(f, "{name}(): {reason}"),
            Self::InvalidStage { operation } => {
                write!(f, "{operation} is not valid at this resolution boundary")
            }
            Self::Cycle { operation } => write!(f, "cycle while resolving {operation}"),
            Self::UnavailableHistoricalSecret(error) => error.fmt(f),
            Self::UnresolvedAtBoundary { sites } => write!(
                f,
                "{} plan-time value operation(s) remain unresolved at the plan boundary",
                sites.len()
            ),
        }
    }
}
impl<B: BindingAddress> std::error::Error for KnowledgeError<B> {}

/// Diagnostic ownership preserves the original binding domain. This carries
/// errors across non-generic consumers without resolving or rewriting sites.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KnowledgeDiagnostic {
    #[error(transparent)]
    Relative(KnowledgeError<super::RelativeBindingName>),
    #[error(transparent)]
    Absolute(KnowledgeError<AbsoluteBindingAddress>),
}
impl<B: BindingAddress> From<KnowledgeError<B>> for KnowledgeDiagnostic {
    fn from(error: KnowledgeError<B>) -> Self {
        super::binding_address::knowledge_diagnostic(error)
    }
}
impl InsideSecret for KnowledgeDiagnostic {
    fn inside_secret(self) -> Self {
        match self {
            Self::Relative(error) => Self::Relative(error.inside_secret()),
            Self::Absolute(error) => Self::Absolute(error.inside_secret()),
        }
    }
}
impl<B: BindingAddress> InsideSecret for KnowledgeError<B> {
    fn inside_secret(self) -> Self {
        match self {
            Self::Unavailable {
                name,
                reason: _,
                source_line,
            } => Self::Unavailable {
                source_line,
                name,
                reason: SecretDiagnosticError::InsideSecret,
            },
            Self::UnresolvedAtBoundary { mut sites } => {
                for site in &mut sites {
                    site.scope = DiagnosticScope::Secret;
                }
                Self::UnresolvedAtBoundary { sites }
            }
            Self::UnavailableHistoricalSecret(error) => {
                Self::UnavailableHistoricalSecret(error.inside_secret())
            }
            Self::InvalidStage { operation } => Self::InvalidStage { operation },
            Self::Cycle { operation } => Self::Cycle { operation },
        }
    }
}
pub struct KnowledgeContext<
    'c,
    P: PlaceholderKind = NoPlaceholder,
    B: BindingAddress = AbsoluteBindingAddress,
    S: ClassificationStage = Validation,
> {
    inputs: kernel::Inputs<'c, P, B>,
    stage: std::marker::PhantomData<S>,
}
impl<'c, P: PlaceholderKind, B: BindingAddress> KnowledgeContext<'c, P, B, Validation> {
    pub fn validation() -> Self {
        Self {
            inputs: kernel::Inputs::Validation,
            stage: std::marker::PhantomData,
        }
    }
}
impl<'c, P: PlaceholderKind, B: BindingAddress> KnowledgeContext<'c, P, B, Plan> {
    pub fn plan(bindings: &'c ResolvedBindings<P, B>) -> Self {
        Self {
            inputs: kernel::Inputs::Plan(bindings),
            stage: std::marker::PhantomData,
        }
    }
}
impl<'c, P: PlaceholderKind, B: BindingAddress> KnowledgeContext<'c, P, B, Apply> {
    pub fn apply(bindings: &'c ResolvedBindings<P, B>) -> Self {
        Self {
            inputs: kernel::Inputs::Plan(bindings),
            stage: std::marker::PhantomData,
        }
    }
}

mod kernel {
    use super::*;
    pub trait SiteSource<B: BindingAddress> {
        fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>);
    }

    #[derive(Clone)]
    pub(super) enum KnownNode<'a, P: PlaceholderKind, B: BindingAddress> {
        String(&'a ValueString),
        EnumIdentifier(&'a RawEnumIdentifier),
        CanonicalEnum(&'a CanonicalEnumValue),
        Int(i64),
        Float(FiniteF64),
        Bool(bool),
        Duration(Duration),
        List(KnownList<'a, P, B>),
        StringList(&'a [ValueString]),
        Map(KnownMap<'a, P, B>),
        Secret(ProtectedSecret<'a, P, B>),
    }
    #[derive(Clone)]
    pub(super) enum ShapeNode<'a, P: PlaceholderKind, B: BindingAddress, T> {
        List(Vec<Classification<'a, P, B, T>>),
        Map(IndexMap<&'a str, Classification<'a, P, B, T>>),
        Coalesce(CoalescePending<ShapeKnown<'a, P, B, T>, CoalesceAlternative<'a, P, B, T>>),
    }
    /// The alternative is not an active input. Only a coalesce-pending node
    /// owns it, including when the left side already establishes a shape.
    #[derive(Clone)]
    pub(super) struct CoalescePending<L, A> {
        pub value: Box<L>,
        pub alternative: A,
    }
    #[derive(Clone)]
    pub(super) struct CoalesceAlternative<'a, P: PlaceholderKind, B: BindingAddress, T> {
        pub evidence: Box<Classification<'a, P, B, T>>,
    }
    #[derive(Clone)]
    pub(super) enum PendingNode<'a, P: PlaceholderKind, B: BindingAddress, T, I> {
        Ordinary(Vec<I>),
        Secret {
            protected: ProtectedSecret<'a, P, B>,
            inner: Box<Classification<'a, P, B, T>>,
        },
        Coalesce(CoalescePending<Classification<'a, P, B, T>, CoalesceAlternative<'a, P, B, T>>),
    }
    /// Immutable lexical evidence shared only by calls in one classification.
    /// Call arguments and position-specific site prefixes remain in each Input.
    pub(super) struct FunctionAnalysis<'a, B: BindingAddress, T = Pending<'a, ParserPlaceholder, B>> {
        pub body: Classification<'a, ParserPlaceholder, B, T>,
        pub locals: Vec<Classification<'a, ParserPlaceholder, B, T>>,
        pub apply_required: bool,
        pub failure: Option<KnowledgeError<B>>,
        pub permits_static_evaluation: bool,
    }
    #[derive(Clone)]
    pub(super) enum InputValue<'a, P: PlaceholderKind, B: BindingAddress> {
        Value(Classification<'a, P, B>),
        Lexical(Arc<FunctionAnalysis<'a, B>>),
    }
    #[derive(Clone)]
    pub(super) struct Input<'a, P: PlaceholderKind, B: BindingAddress> {
        pub step: Option<OperandStep>,
        pub selection: Option<&'a super::super::PathSegment>,
        pub value: InputValue<'a, P, B>,
        pub visible: bool,
        pub guard: Option<PendingGuard>,
        pub apply: bool,
    }
    impl<'a, P: PlaceholderKind, B: BindingAddress> Input<'a, P, B> {
        pub fn value(step: OperandStep, value: Classification<'a, P, B>) -> Self {
            Self {
                step: Some(step),
                selection: None,
                value: InputValue::Value(value),
                visible: true,
                guard: None,
                apply: false,
            }
        }
        pub fn apply_required(&self) -> bool {
            self.visible
                && self.guard.is_none()
                && match &self.value {
                    InputValue::Value(value) => value.apply_required(),
                    InputValue::Lexical(value) => value.apply_required,
                }
        }
        pub fn failure(&self) -> Option<KnowledgeError<B>> {
            if self.guard.is_some() {
                return None;
            }
            match &self.value {
                InputValue::Value(value) => value.failure(),
                InputValue::Lexical(value) => value.failure.clone(),
            }
        }
    }
    #[derive(Clone)]
    pub(super) enum ApplyInputValue<'a, P: PlaceholderKind, B: BindingAddress> {
        Value(Classification<'a, P, B, ApplyPending<'a, P, B>>),
        Lexical(Arc<FunctionAnalysis<'a, B, ApplyPending<'a, ParserPlaceholder, B>>>),
    }
    #[derive(Clone)]
    pub(super) struct ApplyInput<'a, P: PlaceholderKind, B: BindingAddress> {
        pub step: Option<OperandStep>,
        pub selection: Option<&'a super::super::PathSegment>,
        pub value: ApplyInputValue<'a, P, B>,
        pub visible: bool,
        pub guard: Option<PendingGuard>,
    }
    #[derive(Clone)]
    pub(super) enum IterableNode<'a, P: PlaceholderKind, B: BindingAddress, T> {
        Known {
            value: KnownValue<'a, P, B>,
            collection: IterableCollection,
        },
        Shape(ShapeKnown<'a, P, B, T>),
    }
    #[derive(Clone)]
    pub(super) enum IterableCollection {
        List(usize),
        Map(usize),
        StringList(usize),
    }
    impl IterableCollection {
        pub fn len(&self) -> usize {
            match self {
                Self::List(len) | Self::Map(len) | Self::StringList(len) => *len,
            }
        }
    }
    pub(super) enum Inputs<'c, P: PlaceholderKind, B: BindingAddress> {
        Validation,
        Plan(&'c ResolvedBindings<P, B>),
    }
}

impl super::secret_payload::SealedSecretReadResult for kernel::IterableCollection {}
impl SecretReadResult for kernel::IterableCollection {}
impl<P: PlaceholderKind, B: BindingAddress> ProtectedSecret<'_, P, B> {
    pub(crate) fn integer_operand(&self) -> Option<i64> {
        self.payload.visit(|value| {
            value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .ok()
                .and_then(|value| value.into_known().ok())
                .and_then(|value| {
                    value.visit(
                        |value| match value {
                            KnownView::Int(value) => Some(value),
                            _ => None,
                        },
                        |secret| secret.integer_operand(),
                    )
                })
        })
    }
    pub(crate) fn interpolation_literal(&self) -> Option<ValueString> {
        self.payload
            .try_project_plaintext(|value| {
                value
                    .knowledge(&KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .ok()
                    .and_then(|value| value.into_known().ok())
                    .as_ref()
                    .and_then(super::value_into_literal)
                    .map(|(text, _)| text)
                    .ok_or(())
            })
            .ok()
    }
}
impl ProtectedSecret<'_, NoPlaceholder, AbsoluteBindingAddress> {
    pub(crate) fn require_known_for_serialization(
        &self,
    ) -> Result<(), crate::value::SerializationError> {
        self.payload.try_inspect(|value| {
            super::require_known_value(
                value
                    .knowledge(&KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .map_err(super::knowledge_serialization_error)?,
            )
            .map(|_| ())
        })
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> KnownValue<'a, P, B> {
    /// The apply-specific projection accepts only `Knowledge<Apply>`. A fully
    /// known, stage-independent proof can also come from `Classification::into_known`.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::resource::{Value, ConcreteValue};
    /// use carina_core::resource::knowledge::{KnowledgeContext, KnownValue};
    /// let value: Value = Value::Concrete(ConcreteValue::Int(1));
    /// let validation = value.knowledge(&KnowledgeContext::validation()).unwrap();
    /// let _ = KnownValue::from_apply(validation);
    /// ```
    pub fn from_apply(knowledge: Knowledge<'a, P, B, Apply>) -> Result<Self, KnowledgeError<B>> {
        knowledge.into_classification().into_known()
    }

    pub(crate) fn is_secret(&self) -> bool {
        self.visit(|_| false, |_| true)
    }
    pub(crate) fn visit<R>(
        &self,
        ordinary: impl FnOnce(KnownView<'_, P, B>) -> R,
        secret: impl FnOnce(&ProtectedSecret<'_, P, B>) -> R,
    ) -> R {
        use kernel::KnownNode as N;
        ordinary(match &self.node {
            N::String(v) => KnownView::String(v),
            N::EnumIdentifier(v) => KnownView::EnumIdentifier(v),
            N::CanonicalEnum(v) => KnownView::CanonicalEnum(v),
            N::Int(v) => KnownView::Int(*v),
            N::Float(v) => KnownView::Float(*v),
            N::Bool(v) => KnownView::Bool(*v),
            N::Duration(v) => KnownView::Duration(*v),
            N::List(v) => KnownView::List(v),
            N::StringList(v) => KnownView::StringList(v),
            N::Map(v) => KnownView::Map(v),
            N::Secret(v) => return secret(v),
        })
    }
    pub fn to_value(&self) -> Value<P, B> {
        self.visit(
            |view| {
                Value::Concrete(match view {
                    KnownView::String(v) => ConcreteValue::String(v.clone()),
                    KnownView::EnumIdentifier(v) => ConcreteValue::EnumIdentifier(v.clone()),
                    KnownView::CanonicalEnum(v) => ConcreteValue::CanonicalEnum(v.clone()),
                    KnownView::Int(v) => ConcreteValue::Int(v),
                    KnownView::Float(v) => ConcreteValue::Float(v),
                    KnownView::Bool(v) => ConcreteValue::Bool(v),
                    KnownView::Duration(v) => ConcreteValue::Duration(v),
                    KnownView::List(v) => {
                        ConcreteValue::List(v.iter().map(|item| item.to_value()).collect())
                    }
                    KnownView::StringList(v) => ConcreteValue::StringList(v.to_vec()),
                    KnownView::Map(v) => ConcreteValue::Map(
                        v.iter()
                            .map(|(k, v)| (k.to_owned(), v.to_value()))
                            .collect(),
                    ),
                })
            },
            |secret| Value::Deferred(DeferredValue::Secret(secret.payload.clone())),
        )
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> KnownList<'a, P, B> {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &KnownValue<'a, P, B>> {
        self.items.iter()
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> KnownMap<'a, P, B> {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &KnownValue<'a, P, B>)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress, T> ShapeKnown<'a, P, B, T> {
    pub fn len(&self) -> usize {
        match &self.node {
            kernel::ShapeNode::List(v) => v.len(),
            kernel::ShapeNode::Map(v) => v.len(),
            kernel::ShapeNode::Coalesce(pending) => pending.value.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub(crate) fn visit<R>(
        &self,
        ordinary: impl FnOnce(ShapeView<'_, P, B, T>) -> R,
        _secret: impl FnOnce(&ProtectedSecret<'_, P, B>) -> R,
    ) -> R {
        match &self.node {
            kernel::ShapeNode::List(v) => ordinary(ShapeView::List(v)),
            kernel::ShapeNode::Map(v) => ordinary(ShapeView::Map(v)),
            kernel::ShapeNode::Coalesce(pending) => pending.value.visit(ordinary, _secret),
        }
    }
    pub fn to_value(&self) -> Value<P, B> {
        self.source.clone()
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> ShapeKnown<'a, P, B> {
    fn permits_static_evaluation(&self) -> bool {
        match &self.node {
            kernel::ShapeNode::List(items) => {
                items.iter().all(Classification::permits_static_evaluation)
            }
            kernel::ShapeNode::Map(items) => items
                .values()
                .all(Classification::permits_static_evaluation),
            kernel::ShapeNode::Coalesce(pending) => {
                pending.value.permits_static_evaluation()
                    && pending.alternative.evidence.permits_static_evaluation()
            }
        }
    }
    fn failure(&self) -> Option<KnowledgeError<B>> {
        match &self.node {
            kernel::ShapeNode::List(items) => items.iter().find_map(Classification::failure),
            kernel::ShapeNode::Map(items) => items.values().find_map(Classification::failure),
            kernel::ShapeNode::Coalesce(pending) => pending.value.failure(),
        }
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> Pending<'a, P, B> {
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn reason(&self) -> &PendingReason {
        &self.reason
    }
    pub fn sites(&self) -> PendingSites<'_, B> {
        PendingSites::new(self)
    }
    pub(crate) fn visit<R>(
        &self,
        ordinary: impl FnOnce(Phase, &PendingReason, PendingSites<'_, B>) -> R,
        secret: impl FnOnce(&ProtectedSecret<'_, P, B>) -> R,
    ) -> R {
        match &self.node {
            kernel::PendingNode::Secret { protected, .. } => secret(protected),
            kernel::PendingNode::Ordinary(_) | kernel::PendingNode::Coalesce(_) => {
                ordinary(self.phase, &self.reason, self.sites())
            }
        }
    }
    pub(crate) fn to_value(&self) -> Value<P, B> {
        self.source.clone()
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> Classification<'a, P, B> {
    pub(crate) fn module_iteration_violation(&self) -> Option<PhaseViolation<B>> {
        match self {
            Self::Known(_) => None,
            Self::ShapeKnown(_) | Self::Failed(_) => None,
            Self::Pending(value) => value
                .sites()
                .find(|site| site.guard == PendingGuard::Required && site.phase() == Phase::Apply)
                .map(|site| PhaseViolation {
                    required: Requirement::ShapeForPlan,
                    site: Box::new(site),
                    source: None,
                }),
        }
    }
    /// All known-value gates share this exhaustive narrowing. Rejection does
    /// not depend on whether a diagnostic walk finds an unresolved child.
    pub(crate) fn project_known<E>(
        self,
        reject: impl FnOnce(&Self) -> E,
    ) -> Result<KnownValue<'a, P, B>, E> {
        match self {
            Self::Known(value) => Ok(value),
            other @ (Self::ShapeKnown(_) | Self::Pending(_) | Self::Failed(_)) => {
                Err(reject(&other))
            }
        }
    }
    pub fn into_known(self) -> Result<KnownValue<'a, P, B>, KnowledgeError<B>> {
        self.project_known(|value| {
            value.failure().unwrap_or(KnowledgeError::InvalidStage {
                operation: "known-value projection",
            })
        })
    }
    /// Scheduling consumes cached classification; protected inputs retain their
    /// existing computation policy without exposing their payload.
    pub(crate) fn permits_static_evaluation(&self) -> bool {
        match self {
            Self::Known(_) => true,
            Self::ShapeKnown(value) => value.permits_static_evaluation(),
            Self::Pending(value) => {
                matches!(value.reason, PendingReason::Evaluation(_))
                    && match &value.node {
                        kernel::PendingNode::Ordinary(inputs) => {
                            inputs.iter().filter(|input| input.visible).all(|input| {
                                match &input.value {
                                    kernel::InputValue::Value(value) => {
                                        value.permits_static_evaluation()
                                    }
                                    kernel::InputValue::Lexical(value) => {
                                        value.permits_static_evaluation
                                    }
                                }
                            }) && inputs.iter().all(|input| input.failure().is_none())
                        }
                        kernel::PendingNode::Secret { inner, .. } => {
                            inner.permits_static_evaluation()
                        }
                        kernel::PendingNode::Coalesce(pending) => {
                            pending.value.permits_static_evaluation()
                                && pending.alternative.evidence.permits_static_evaluation()
                        }
                    }
            }
            Self::Failed(_) => false,
        }
    }
    pub fn to_value(&self) -> Value<P, B> {
        match self {
            Self::Known(v) => v.to_value(),
            Self::ShapeKnown(v) => v.to_value(),
            Self::Pending(v) => v.to_value(),
            Self::Failed(v) => v.source.clone(),
        }
    }
    pub(crate) fn failure(&self) -> Option<KnowledgeError<B>> {
        match self {
            Self::Known(_) => None,
            Self::ShapeKnown(value) => value.failure(),
            Self::Pending(value) => match &value.node {
                kernel::PendingNode::Ordinary(inputs) => {
                    inputs.iter().find_map(kernel::Input::failure)
                }
                kernel::PendingNode::Coalesce(pending) => pending.value.failure(),
                kernel::PendingNode::Secret { inner, .. } => {
                    inner.failure().map(InsideSecret::inside_secret)
                }
            },
            Self::Failed(value) => Some(value.error()),
        }
    }
    pub(crate) fn apply_required(&self) -> bool {
        match self {
            Self::Known(_) => false,
            Self::Failed(_) => false,
            Self::ShapeKnown(value) => value.apply_required,
            Self::Pending(value) => value.phase == Phase::Apply,
        }
    }
}
/// A schema deferment can only be minted from an actual pending proof.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingFit {
    _private: (),
}

type CoalesceOperands<'s, 'a, P, B, T> = (
    &'s Classification<'a, P, B, T>,
    &'s Classification<'a, P, B, T>,
);

pub(crate) struct PendingStructure<'s, 'a, P: PlaceholderKind, B: BindingAddress, T> {
    source: &'a Value<P, B>,
    wait_on: &'s WaitOn<B>,
    coalesce: Option<CoalesceOperands<'s, 'a, P, B, T>>,
    branches: Option<Vec<&'s Classification<'a, P, B, T>>>,
}
pub(crate) trait StructuralPending<'a, P: PlaceholderKind + 'a, B: BindingAddress>:
    Sized
{
    fn structure(&self) -> PendingStructure<'_, 'a, P, B, Self>;
    fn source(&self) -> &'a Value<P, B> {
        self.structure().source
    }
    fn arithmetic(&self) -> bool {
        matches!(
            self.structure().wait_on,
            WaitOn::Evaluation(EvaluationKind::IntegerArithmetic)
        )
    }
    fn fit(&self) -> PendingFit {
        PendingFit { _private: () }
    }
    fn coalesce(&self) -> Option<CoalesceOperands<'_, 'a, P, B, Self>> {
        self.structure().coalesce
    }
    fn branches(&self) -> Option<Vec<&Classification<'a, P, B, Self>>> {
        self.structure().branches
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> StructuralPending<'a, P, B> for Pending<'a, P, B> {
    fn structure(&self) -> PendingStructure<'_, 'a, P, B, Self> {
        let (coalesce, branches) = match &self.node {
            kernel::PendingNode::Coalesce(pending) => (
                Some((
                    pending.value.as_ref(),
                    pending.alternative.evidence.as_ref(),
                )),
                None,
            ),
            kernel::PendingNode::Secret { .. } => (None, None),
            kernel::PendingNode::Ordinary(inputs) => (
                None,
                matches!(self.wait_on, WaitOn::Evaluation(EvaluationKind::ModuleIf)).then(|| {
                    inputs
                        .iter()
                        .filter_map(|input| match (&input.step, &input.value) {
                            (
                                Some(OperandStep::Then | OperandStep::Else),
                                kernel::InputValue::Value(value),
                            ) => Some(value),
                            _ => None,
                        })
                        .collect()
                }),
            ),
        };
        PendingStructure {
            source: self.source,
            wait_on: &self.wait_on,
            coalesce,
            branches,
        }
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> StructuralPending<'a, P, B>
    for ApplyPending<'a, P, B>
{
    fn structure(&self) -> PendingStructure<'_, 'a, P, B, Self> {
        // Authored alternatives have already been checked before projection.
        PendingStructure {
            source: self.source,
            wait_on: &self.wait_on,
            coalesce: None,
            branches: None,
        }
    }
}

/// Borrowed evidence for the schema/path consumers, including children of an
/// all-known collection. This is crate-private and never accepts a raw Value.
pub(crate) enum ClassifiedRef<'s, 'a, P: PlaceholderKind, B: BindingAddress, T = Pending<'a, P, B>>
{
    Known(&'s KnownValue<'a, P, B>),
    Shape(&'s ShapeKnown<'a, P, B, T>),
    Pending(&'s T),
    Failed(&'s AttemptedFailure<'a, P, B>),
}
impl<P: PlaceholderKind, B: BindingAddress, T> Copy for ClassifiedRef<'_, '_, P, B, T> {}
impl<P: PlaceholderKind, B: BindingAddress, T> Clone for ClassifiedRef<'_, '_, P, B, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<'s, 'a, P: PlaceholderKind, B: BindingAddress, T> From<&'s Classification<'a, P, B, T>>
    for ClassifiedRef<'s, 'a, P, B, T>
{
    fn from(value: &'s Classification<'a, P, B, T>) -> Self {
        match value {
            Classification::Known(value) => Self::Known(value),
            Classification::ShapeKnown(value) => Self::Shape(value),
            Classification::Pending(value) => Self::Pending(value),
            Classification::Failed(value) => Self::Failed(value),
        }
    }
}
impl<'s, 'a, P: PlaceholderKind, B: BindingAddress, T: StructuralPending<'a, P, B>>
    ClassifiedRef<'s, 'a, P, B, T>
{
    pub(crate) fn diagnostic_value(self) -> &'a Value<P, B> {
        match self {
            Self::Known(value) => value.source,
            Self::Shape(value) => value.source,
            Self::Pending(value) => value.source(),
            Self::Failed(value) => value.source,
        }
    }
    pub(crate) fn type_name(self) -> String {
        self.diagnostic_value().type_name()
    }
    pub(crate) fn concrete(self) -> Option<super::ConcreteValueRef<'a, P, B>> {
        match self {
            Self::Known(value) => match value.node {
                kernel::KnownNode::Secret(_) => None,
                kernel::KnownNode::String(_)
                | kernel::KnownNode::EnumIdentifier(_)
                | kernel::KnownNode::CanonicalEnum(_)
                | kernel::KnownNode::Int(_)
                | kernel::KnownNode::Float(_)
                | kernel::KnownNode::Bool(_)
                | kernel::KnownNode::Duration(_)
                | kernel::KnownNode::StringList(_)
                | kernel::KnownNode::List(_)
                | kernel::KnownNode::Map(_) => value.source.as_concrete(),
            },
            Self::Shape(value) => match &value.node {
                kernel::ShapeNode::Coalesce(pending) => {
                    ClassifiedRef::Shape(&pending.value).concrete()
                }
                kernel::ShapeNode::List(_) | kernel::ShapeNode::Map(_) => {
                    value.source.as_concrete()
                }
            },
            Self::Pending(_) | Self::Failed(_) => None,
        }
    }
    pub(crate) fn secret(self) -> Option<&'a SecretPayload<P, B>> {
        match self {
            Self::Known(value) => match &value.node {
                kernel::KnownNode::Secret(secret) => Some(secret.payload),
                kernel::KnownNode::String(_)
                | kernel::KnownNode::EnumIdentifier(_)
                | kernel::KnownNode::CanonicalEnum(_)
                | kernel::KnownNode::Int(_)
                | kernel::KnownNode::Float(_)
                | kernel::KnownNode::Bool(_)
                | kernel::KnownNode::Duration(_)
                | kernel::KnownNode::StringList(_)
                | kernel::KnownNode::List(_)
                | kernel::KnownNode::Map(_) => None,
            },
            Self::Shape(_) => None,
            Self::Pending(_) | Self::Failed(_) => match self.diagnostic_value() {
                Value::Deferred(DeferredValue::Secret(payload)) => Some(payload),
                _ => None,
            },
        }
    }
    pub(crate) fn list(self) -> Option<Vec<Self>> {
        match self {
            Self::Known(value) => match &value.node {
                kernel::KnownNode::List(items) => Some(items.iter().map(Self::Known).collect()),
                kernel::KnownNode::Secret(_) => None,
                kernel::KnownNode::String(_)
                | kernel::KnownNode::EnumIdentifier(_)
                | kernel::KnownNode::CanonicalEnum(_)
                | kernel::KnownNode::Int(_)
                | kernel::KnownNode::Float(_)
                | kernel::KnownNode::Bool(_)
                | kernel::KnownNode::Duration(_)
                | kernel::KnownNode::StringList(_)
                | kernel::KnownNode::Map(_) => None,
            },
            Self::Shape(value) => match &value.node {
                kernel::ShapeNode::List(items) => Some(items.iter().map(Self::from).collect()),
                kernel::ShapeNode::Map(_) => None,
                kernel::ShapeNode::Coalesce(pending) => Self::Shape(&pending.value).list(),
            },
            Self::Pending(_) | Self::Failed(_) => None,
        }
    }
    pub(crate) fn map(self) -> Option<IndexMap<&'a str, Self>> {
        match self {
            Self::Known(value) => match &value.node {
                kernel::KnownNode::Map(items) => Some(
                    items
                        .entries
                        .iter()
                        .map(|(k, v)| (*k, Self::Known(v)))
                        .collect(),
                ),
                kernel::KnownNode::Secret(_) => None,
                kernel::KnownNode::String(_)
                | kernel::KnownNode::EnumIdentifier(_)
                | kernel::KnownNode::CanonicalEnum(_)
                | kernel::KnownNode::Int(_)
                | kernel::KnownNode::Float(_)
                | kernel::KnownNode::Bool(_)
                | kernel::KnownNode::Duration(_)
                | kernel::KnownNode::StringList(_)
                | kernel::KnownNode::List(_) => None,
            },
            Self::Shape(value) => match &value.node {
                kernel::ShapeNode::Map(items) => {
                    Some(items.iter().map(|(k, v)| (*k, Self::from(v))).collect())
                }
                kernel::ShapeNode::List(_) => None,
                kernel::ShapeNode::Coalesce(pending) => Self::Shape(&pending.value).map(),
            },
            Self::Pending(_) | Self::Failed(_) => None,
        }
    }
    pub(crate) fn coalesce(self) -> Option<(Self, Self)> {
        match self {
            Self::Shape(value) => match &value.node {
                kernel::ShapeNode::Coalesce(pending) => Some((
                    Self::Shape(&pending.value),
                    Self::from(pending.alternative.evidence.as_ref()),
                )),
                kernel::ShapeNode::List(_) | kernel::ShapeNode::Map(_) => None,
            },
            Self::Pending(value) => value
                .coalesce()
                .map(|(left, right)| (Self::from(left), Self::from(right))),
            Self::Known(_) | Self::Failed(_) => None,
        }
    }
    pub(crate) fn branches(self) -> Option<Vec<Self>> {
        match self {
            Self::Pending(value) => value
                .branches()
                .map(|values| values.into_iter().map(Self::from).collect()),
            Self::Known(_) | Self::Shape(_) | Self::Failed(_) => None,
        }
    }
}
// The cursor keeps one mutable prefix. Only a yielded event materializes a
// LeafPath; no classified ancestor stores a copy of descendant sites.
use kernel::SiteSource;

struct SiteEdge<'s, B: BindingAddress> {
    value: &'s dyn SiteSource<B>,
    segment: Option<crate::schema::LeafPathSegment>,
    operand: Option<OperandStep>,
    guard: Option<PendingGuard>,
    apply: bool,
    secret: bool,
}
enum SiteFrame<'s, B: BindingAddress> {
    Enter(SiteEdge<'s, B>),
    Restore {
        path: usize,
        operand: usize,
        guard: PendingGuard,
        apply: bool,
        scope: DiagnosticScope,
    },
    Event {
        wait_on: WaitOn<B>,
        phase: Phase,
        reason: PendingReason,
    },
}
pub struct PendingSites<'s, B: BindingAddress = AbsoluteBindingAddress> {
    stack: Vec<SiteFrame<'s, B>>,
    path: Vec<crate::schema::LeafPathSegment>,
    operand: Vec<OperandStep>,
    guard: PendingGuard,
    apply: bool,
    scope: DiagnosticScope,
}
impl<'s, B: BindingAddress> PendingSites<'s, B> {
    fn new(value: &'s dyn SiteSource<B>) -> Self {
        Self {
            stack: vec![SiteFrame::Enter(SiteEdge {
                value,
                segment: None,
                operand: None,
                guard: None,
                apply: false,
                secret: false,
            })],
            path: Vec::new(),
            operand: Vec::new(),
            guard: PendingGuard::Required,
            apply: false,
            scope: DiagnosticScope::Public,
        }
    }
}
impl<B: BindingAddress> Iterator for PendingSites<'_, B> {
    type Item = PendingSite<B>;
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(frame) = self.stack.pop() {
            match frame {
                SiteFrame::Enter(edge) => {
                    self.stack.push(SiteFrame::Restore {
                        path: self.path.len(),
                        operand: self.operand.len(),
                        guard: self.guard,
                        apply: self.apply,
                        scope: self.scope,
                    });
                    if let Some(segment) = edge.segment {
                        self.path.push(segment);
                    }
                    if let Some(operand) = edge.operand {
                        self.operand.push(operand);
                    }
                    if let Some(guard) = edge.guard {
                        self.guard = guard;
                    }
                    self.apply |= edge.apply;
                    if edge.secret {
                        self.scope = DiagnosticScope::Secret;
                    }
                    edge.value.enqueue(self);
                }
                SiteFrame::Restore {
                    path,
                    operand,
                    guard,
                    apply,
                    scope,
                } => {
                    self.path.truncate(path);
                    self.operand.truncate(operand);
                    self.guard = guard;
                    self.apply = apply;
                    self.scope = scope;
                }
                SiteFrame::Event {
                    wait_on,
                    phase,
                    reason,
                } => {
                    return Some(PendingSite {
                        path: LeafPath::from_segments(self.path.iter().cloned()),
                        operand: self.operand.clone(),
                        wait_on,
                        phase: if self.apply { Phase::Apply } else { phase },
                        reason,
                        scope: self.scope,
                        guard: self.guard,
                    });
                }
            }
        }
        None
    }
}
impl<P: PlaceholderKind, B: BindingAddress, T: SiteSource<B>> SiteSource<B>
    for Classification<'_, P, B, T>
{
    fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>) {
        match self {
            Self::Known(_) | Self::Failed(_) => {}
            Self::ShapeKnown(value) => value.enqueue(sites),
            Self::Pending(value) => value.enqueue(sites),
        }
    }
}
impl<P: PlaceholderKind, B: BindingAddress, T: SiteSource<B>> SiteSource<B>
    for ShapeKnown<'_, P, B, T>
{
    fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>) {
        let stack = &mut sites.stack;
        use crate::schema::LeafPathSegment;
        match &self.node {
            kernel::ShapeNode::List(items) => {
                for (index, value) in items.iter().enumerate().rev() {
                    stack.push(SiteFrame::Enter(SiteEdge {
                        value,
                        segment: Some(LeafPathSegment::ListIndex(index)),
                        operand: None,
                        guard: None,
                        apply: false,
                        secret: false,
                    }));
                }
            }
            kernel::ShapeNode::Map(items) => {
                for (key, value) in items.iter().rev() {
                    stack.push(SiteFrame::Enter(SiteEdge {
                        value,
                        segment: Some(LeafPathSegment::MapKey((*key).to_owned().into())),
                        operand: None,
                        guard: None,
                        apply: false,
                        secret: false,
                    }));
                }
            }
            kernel::ShapeNode::Coalesce(pending) => enqueue_coalesce(
                pending,
                if self.apply_required {
                    Phase::Apply
                } else {
                    Phase::Plan
                },
                stack,
            ),
        }
    }
}
fn enqueue_coalesce<
    's,
    P: PlaceholderKind,
    B: BindingAddress,
    T: SiteSource<B>,
    L: SiteSource<B>,
>(
    pending: &'s kernel::CoalescePending<L, kernel::CoalesceAlternative<'_, P, B, T>>,
    phase: Phase,
    stack: &mut Vec<SiteFrame<'s, B>>,
) {
    stack.push(SiteFrame::Enter(SiteEdge {
        value: pending.alternative.evidence.as_ref(),
        segment: None,
        operand: Some(OperandStep::CoalesceFallback),
        guard: Some(PendingGuard::CoalesceFallback),
        apply: phase == Phase::Apply,
        secret: false,
    }));
    stack.push(SiteFrame::Enter(SiteEdge {
        value: pending.value.as_ref(),
        segment: None,
        operand: Some(OperandStep::CoalesceLeft),
        guard: None,
        apply: false,
        secret: false,
    }));
    stack.push(SiteFrame::Event {
        wait_on: WaitOn::Evaluation(EvaluationKind::Coalesce),
        reason: PendingReason::Evaluation(EvaluationKind::Coalesce),
        phase,
    });
}
// Selection tokens are borrowed while classifying. Leaf-path fragments are
// materialized only by the on-demand site traversal, alongside collection keys.
fn selection_site_segment(step: &super::PathSegment) -> Option<crate::schema::LeafPathSegment> {
    use super::{PathSegment, Subscript};
    use crate::schema::LeafPathSegment;
    match step {
        PathSegment::Field { name } => Some(LeafPathSegment::MapKey(name.clone().into())),
        PathSegment::Subscript {
            index: Subscript::Str { key },
        } => Some(LeafPathSegment::MapKey(key.clone())),
        PathSegment::Subscript {
            index: Subscript::Int { index },
        } => usize::try_from(*index).ok().map(LeafPathSegment::ListIndex),
    }
}
trait InputSite<B: BindingAddress> {
    fn edge(&self) -> Option<SiteEdge<'_, B>>;
}
impl<P: PlaceholderKind, B: BindingAddress> InputSite<B> for kernel::Input<'_, P, B> {
    fn edge(&self) -> Option<SiteEdge<'_, B>> {
        if !self.visible {
            return None;
        }
        let value: &dyn SiteSource<B> = match &self.value {
            kernel::InputValue::Value(value) => value,
            kernel::InputValue::Lexical(value) => value.as_ref(),
        };
        Some(SiteEdge {
            value,
            segment: self.selection.and_then(selection_site_segment),
            operand: self.step.clone(),
            guard: self.guard,
            apply: self.apply,
            secret: false,
        })
    }
}
impl<P: PlaceholderKind, B: BindingAddress> InputSite<B> for kernel::ApplyInput<'_, P, B> {
    fn edge(&self) -> Option<SiteEdge<'_, B>> {
        if !self.visible {
            return None;
        }
        let value: &dyn SiteSource<B> = match &self.value {
            kernel::ApplyInputValue::Value(value) => value,
            kernel::ApplyInputValue::Lexical(value) => value.as_ref(),
        };
        Some(SiteEdge {
            value,
            segment: self.selection.and_then(selection_site_segment),
            operand: self.step.clone(),
            guard: self.guard,
            apply: false,
            secret: false,
        })
    }
}
fn enqueue_inputs<'s, B: BindingAddress, I: InputSite<B>>(
    inputs: &'s [I],
    wait_on: &WaitOn<B>,
    phase: Phase,
    reason: &PendingReason,
    stack: &mut Vec<SiteFrame<'s, B>>,
) {
    for edge in inputs.iter().rev().filter_map(InputSite::edge) {
        stack.push(SiteFrame::Enter(edge));
    }
    stack.push(SiteFrame::Event {
        wait_on: wait_on.clone(),
        phase,
        reason: reason.clone(),
    });
}
impl<'a, B: BindingAddress, T: SiteSource<B>> SiteSource<B> for kernel::FunctionAnalysis<'a, B, T> {
    fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>) {
        for (index, local) in self.locals.iter().enumerate().rev() {
            sites.stack.push(SiteFrame::Enter(SiteEdge {
                value: local,
                segment: None,
                operand: Some(OperandStep::FunctionLocal(index)),
                guard: None,
                apply: false,
                secret: false,
            }));
        }
        sites.stack.push(SiteFrame::Enter(SiteEdge {
            value: &self.body,
            segment: None,
            operand: Some(OperandStep::FunctionBody),
            guard: None,
            apply: false,
            secret: false,
        }));
    }
}
impl<P: PlaceholderKind, B: BindingAddress> SiteSource<B> for Pending<'_, P, B> {
    fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>) {
        let stack = &mut sites.stack;
        match &self.node {
            kernel::PendingNode::Coalesce(pending) => enqueue_coalesce(pending, self.phase, stack),
            kernel::PendingNode::Ordinary(inputs) => {
                enqueue_inputs(inputs, &self.wait_on, self.phase, &self.reason, stack)
            }
            kernel::PendingNode::Secret { inner, .. } => stack.push(SiteFrame::Enter(SiteEdge {
                value: inner.as_ref(),
                segment: None,
                operand: None,
                guard: None,
                apply: false,
                secret: true,
            })),
        }
    }
}
impl<P: PlaceholderKind, B: BindingAddress> SiteSource<B> for ApplyPending<'_, P, B> {
    fn enqueue<'s>(&'s self, sites: &mut PendingSites<'s, B>) {
        let stack = &mut sites.stack;
        match &self.node {
            kernel::PendingNode::Coalesce(pending) => {
                enqueue_coalesce(pending, Phase::Apply, stack)
            }
            kernel::PendingNode::Ordinary(inputs) => {
                enqueue_inputs(inputs, &self.wait_on, Phase::Apply, &self.reason, stack)
            }
            kernel::PendingNode::Secret { inner, .. } => stack.push(SiteFrame::Enter(SiteEdge {
                value: inner.as_ref(),
                segment: None,
                operand: None,
                guard: None,
                apply: false,
                secret: true,
            })),
        }
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> PlanIterable<'a, P, B> {
    pub(crate) fn for_expansion(&self) -> Value<P, B> {
        match &self.node {
            kernel::IterableNode::Known { value, .. } => value.to_value(),
            kernel::IterableNode::Shape(value) => value.visit(
                |view| match view {
                    ShapeView::List(items) => Value::Concrete(ConcreteValue::List(
                        items.iter().map(Classification::to_value).collect(),
                    )),
                    ShapeView::Map(items) => Value::Concrete(ConcreteValue::Map(
                        items
                            .iter()
                            .map(|(key, value)| ((*key).to_owned(), value.to_value()))
                            .collect(),
                    )),
                },
                |secret| Value::Deferred(DeferredValue::Secret(secret.payload.clone())),
            ),
        }
    }

    pub(crate) fn visit<R>(
        &self,
        known: impl FnOnce(KnownView<'_, P, B>) -> R,
        shape: impl FnOnce(ShapeView<'_, P, B, Pending<'a, P, B>>) -> R,
        secret: impl FnOnce(&ProtectedSecret<'_, P, B>) -> R,
    ) -> R {
        match &self.node {
            kernel::IterableNode::Known { value, .. } => value.visit(known, secret),
            kernel::IterableNode::Shape(value) => value.visit(shape, secret),
        }
    }
    pub(crate) fn source_value(&self) -> Value<P, B> {
        match &self.node {
            kernel::IterableNode::Known { value, .. } => value.source.clone(),
            kernel::IterableNode::Shape(value) => value.source.clone(),
        }
    }
    pub fn from_known(value: KnownValue<'a, P, B>) -> Option<Self> {
        use kernel::IterableCollection as C;
        let collection = value.visit(
            |view| match view {
                KnownView::List(items) => Some(C::List(items.len())),
                KnownView::Map(items) => Some(C::Map(items.len())),
                KnownView::StringList(items) => Some(C::StringList(items.len())),
                KnownView::String(_)
                | KnownView::EnumIdentifier(_)
                | KnownView::CanonicalEnum(_)
                | KnownView::Int(_)
                | KnownView::Float(_)
                | KnownView::Bool(_)
                | KnownView::Duration(_) => None,
            },
            |secret| {
                secret.payload.visit(|value| match value.as_concrete() {
                    Some(super::ConcreteValueRef::List(items)) => Some(C::List(items.len())),
                    Some(super::ConcreteValueRef::Map(items)) => Some(C::Map(items.len())),
                    Some(super::ConcreteValueRef::StringList(items)) => {
                        Some(C::StringList(items.len()))
                    }
                    Some(
                        super::ConcreteValueRef::String(_)
                        | super::ConcreteValueRef::EnumIdentifier(_)
                        | super::ConcreteValueRef::CanonicalEnum(_)
                        | super::ConcreteValueRef::Int(_)
                        | super::ConcreteValueRef::Float(_)
                        | super::ConcreteValueRef::Bool(_)
                        | super::ConcreteValueRef::Duration(_),
                    )
                    | None => None,
                })
            },
        )?;
        Some(Self {
            node: kernel::IterableNode::Known { value, collection },
        })
    }
    pub fn from_shape(value: ShapeKnown<'a, P, B>) -> Result<Self, KnowledgeError<B>> {
        if let Some(error) = value.failure() {
            return Err(error);
        }
        Ok(Self {
            node: kernel::IterableNode::Shape(value),
        })
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress, T> PlanIterable<'a, P, B, T> {
    pub fn len(&self) -> usize {
        match &self.node {
            kernel::IterableNode::Shape(v) => v.len(),
            kernel::IterableNode::Known { collection, .. } => collection.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> ApplyPending<'a, P, B> {
    pub fn reason(&self) -> &PendingReason {
        &self.reason
    }
    pub fn sites(&self) -> PendingSites<'_, B> {
        PendingSites::new(self)
    }
    pub fn to_value(&self) -> Value<P, B> {
        self.source.clone()
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress> PlanResolved<'a, P, B> {
    pub fn knowledge(&self) -> &Classification<'a, P, B, ApplyPending<'a, P, B>> {
        &self.knowledge
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress, T: SiteSource<B>> Classification<'a, P, B, T> {
    pub fn sites(&self) -> PendingSites<'_, B> {
        PendingSites::new(self)
    }
}
impl<'a, P: PlaceholderKind, B: BindingAddress, T: SiteSource<B>> ShapeKnown<'a, P, B, T> {
    pub fn sites(&self) -> PendingSites<'_, B> {
        PendingSites::new(self)
    }
}
type ProjectedFunction<'a, B> =
    Arc<kernel::FunctionAnalysis<'a, B, ApplyPending<'a, ParserPlaceholder, B>>>;
type ProjectedFunctions<'a, B> = HashMap<usize, ProjectedFunction<'a, B>>;

fn project_function<'a, B: BindingAddress>(
    value: Arc<kernel::FunctionAnalysis<'a, B>>,
    on_failed: fn(KnowledgeError<B>) -> Result<(), KnowledgeError<B>>,
    functions: &mut ProjectedFunctions<'a, B>,
) -> Result<ProjectedFunction<'a, B>, KnowledgeError<B>> {
    // An analysis first seen under an inactive alternative may be shared by
    // an active call. Re-check its typed active-failure evidence before reuse.
    if let Some(error) = &value.failure {
        on_failed(error.clone())?;
    }
    let id = Arc::as_ptr(&value) as usize;
    if let Some(projected) = functions.get(&id) {
        return Ok(Arc::clone(projected));
    }
    let projected = Arc::new(kernel::FunctionAnalysis {
        body: project_plan(value.body.clone(), |_| Ok(()), functions)?,
        locals: value
            .locals
            .iter()
            .map(|local| project_plan(local.clone(), |_| Ok(()), functions))
            .collect::<Result<_, _>>()?,
        apply_required: value.apply_required,
        failure: value.failure.clone(),
        permits_static_evaluation: value.permits_static_evaluation,
    });
    functions.insert(id, Arc::clone(&projected));
    Ok(projected)
}

// Only a coalesce-pending node can select the evidence-preserving policy.
impl<'a, P: PlaceholderKind, B: BindingAddress>
    kernel::CoalesceAlternative<'a, P, B, Pending<'a, P, B>>
{
    fn project(
        self,
        functions: &mut ProjectedFunctions<'a, B>,
    ) -> Result<kernel::CoalesceAlternative<'a, P, B, ApplyPending<'a, P, B>>, KnowledgeError<B>>
    {
        Ok(kernel::CoalesceAlternative {
            evidence: Box::new(project_plan(*self.evidence, |_| Ok(()), functions)?),
        })
    }
}

fn project_shape<'a, P: PlaceholderKind, B: BindingAddress>(
    value: ShapeKnown<'a, P, B>,
    on_failed: fn(KnowledgeError<B>) -> Result<(), KnowledgeError<B>>,
    functions: &mut ProjectedFunctions<'a, B>,
) -> Result<ShapeKnown<'a, P, B, ApplyPending<'a, P, B>>, KnowledgeError<B>> {
    Ok(ShapeKnown {
        source: value.source,
        apply_required: true,
        node: match value.node {
            kernel::ShapeNode::List(items) => kernel::ShapeNode::List(
                items
                    .into_iter()
                    .map(|value| project_plan(value, on_failed, functions))
                    .collect::<Result<_, _>>()?,
            ),
            kernel::ShapeNode::Map(items) => kernel::ShapeNode::Map(
                items
                    .into_iter()
                    .map(|(key, value)| Ok((key, project_plan(value, on_failed, functions)?)))
                    .collect::<Result<_, KnowledgeError<B>>>()?,
            ),
            kernel::ShapeNode::Coalesce(pending) => {
                kernel::ShapeNode::Coalesce(kernel::CoalescePending {
                    value: Box::new(project_shape(*pending.value, on_failed, functions)?),
                    alternative: pending.alternative.project(functions)?,
                })
            }
        },
    })
}
fn project_plan<'a, P: PlaceholderKind, B: BindingAddress>(
    value: Classification<'a, P, B>,
    on_failed: fn(KnowledgeError<B>) -> Result<(), KnowledgeError<B>>,
    functions: &mut ProjectedFunctions<'a, B>,
) -> Result<Classification<'a, P, B, ApplyPending<'a, P, B>>, KnowledgeError<B>> {
    Ok(match value {
        Classification::Known(value) => Classification::Known(value),
        Classification::Failed(value) => {
            on_failed(value.error())?;
            Classification::Failed(value)
        }
        Classification::ShapeKnown(value) => {
            Classification::ShapeKnown(project_shape(value, on_failed, functions)?)
        }
        Classification::Pending(value) => Classification::Pending(ApplyPending {
            source: value.source,
            wait_on: value.wait_on,
            reason: value.reason,
            node: match value.node {
                kernel::PendingNode::Secret { protected, inner } => kernel::PendingNode::Secret {
                    protected,
                    inner: Box::new(
                        project_plan(*inner, on_failed, functions)
                            .map_err(InsideSecret::inside_secret)?,
                    ),
                },
                kernel::PendingNode::Coalesce(pending) => {
                    kernel::PendingNode::Coalesce(kernel::CoalescePending {
                        value: Box::new(project_plan(*pending.value, on_failed, functions)?),
                        alternative: pending.alternative.project(functions)?,
                    })
                }
                kernel::PendingNode::Ordinary(inputs) => kernel::PendingNode::Ordinary(
                    inputs
                        .into_iter()
                        .map(|input| {
                            Ok(kernel::ApplyInput {
                                step: input.step,
                                selection: input.selection,
                                visible: input.visible,
                                guard: input.guard,
                                value: match input.value {
                                    kernel::InputValue::Value(value) => {
                                        kernel::ApplyInputValue::Value(project_plan(
                                            value,
                                            if input.guard.is_some() {
                                                |_| Ok(())
                                            } else {
                                                on_failed
                                            },
                                            functions,
                                        )?)
                                    }
                                    kernel::InputValue::Lexical(value) => {
                                        kernel::ApplyInputValue::Lexical(project_function(
                                            value,
                                            if input.guard.is_some() {
                                                |_| Ok(())
                                            } else {
                                                on_failed
                                            },
                                            functions,
                                        )?)
                                    }
                                },
                            })
                        })
                        .collect::<Result<_, KnowledgeError<B>>>()?,
                ),
            },
        }),
    })
}
impl<'a, P: PlaceholderKind, B: BindingAddress> TryFrom<Knowledge<'a, P, B, Plan>>
    for PlanResolved<'a, P, B>
{
    type Error = KnowledgeError<B>;
    fn try_from(value: Knowledge<'a, P, B, Plan>) -> Result<Self, Self::Error> {
        let value = value.into_classification();
        let sites = value
            .sites()
            .filter(|site| site.phase() == Phase::Plan)
            .collect::<Vec<_>>();
        let knowledge = project_plan(value, Err, &mut HashMap::new())?;
        if !sites.is_empty() {
            return Err(KnowledgeError::UnresolvedAtBoundary { sites });
        }
        Ok(Self { knowledge })
    }
}
macro_rules! masked_debug {
    ($($name:ident),* $(,)?) => {$ (
        impl<P:PlaceholderKind,B:BindingAddress> fmt::Debug for $name<'_,P,B> {
            fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { f.write_str(stringify!($name)) }
        }
    )*};
}
masked_debug!(
    KnownValue,
    KnownList,
    KnownMap,
    ProtectedSecret,
    Pending,
    ApplyPending,
    PlanResolved,
    AttemptedFailure
);
impl<P: PlaceholderKind, B: BindingAddress, T> fmt::Debug for ShapeKnown<'_, P, B, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShapeKnown")
            .field("length", &self.len())
            .field("pending", &"enumerated on demand")
            .finish()
    }
}
impl<P: PlaceholderKind, B: BindingAddress, T: fmt::Debug> fmt::Debug
    for Classification<'_, P, B, T>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(v) => v.fmt(f),
            Self::ShapeKnown(v) => v.fmt(f),
            Self::Pending(v) => v.fmt(f),
            Self::Failed(v) => v.fmt(f),
        }
    }
}

#[cfg(test)]
thread_local! { static CLASSIFICATION_BUDGET: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn with_classification_budget<R>(limit: usize, operation: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            CLASSIFICATION_BUDGET.set(None);
        }
    }
    CLASSIFICATION_BUDGET.set(Some((0, limit)));
    let _reset = Reset;
    operation()
}
#[cfg(test)]
fn classification_step() {
    CLASSIFICATION_BUDGET.with(|budget| {
        if let Some((count, limit)) = budget.get() {
            assert!(count < limit, "classification exceeded {limit} node steps");
            budget.set(Some((count + 1, limit)));
        }
    });
}

struct Position<'a, B: BindingAddress> {
    references: Vec<WaitOn<B>>,
    analyses: HashMap<usize, Arc<kernel::FunctionAnalysis<'a, B>>>,
    functions: Vec<usize>,
    lexical_body: bool,
}
impl<B: BindingAddress> Position<'_, B> {
    fn root() -> Self {
        Self {
            references: Vec::new(),
            analyses: HashMap::new(),
            functions: Vec::new(),
            lexical_body: false,
        }
    }
}
impl SubstitutionKind {
    fn reason(self) -> PendingReason {
        match self {
            Self::ModuleArgument => PendingReason::ModuleArgument,
            Self::FnParam => PendingReason::FunctionParameter,
            Self::FnLocal => PendingReason::FunctionLocal,
            Self::LoopKey => PendingReason::LoopKey,
            Self::LoopIndex => PendingReason::LoopIndex,
            Self::LoopValue => PendingReason::LoopValue,
            Self::EmptyInterpolation => PendingReason::IncompleteExpression,
        }
    }
}
impl super::evaluable_leaf::Sealed for NoPlaceholder {
    fn substitution_kind(&self) -> SubstitutionKind {
        match *self {}
    }
}
impl super::evaluable_leaf::Sealed for ParserPlaceholder {
    fn substitution_kind(&self) -> SubstitutionKind {
        match self {
            Self::ModuleArgument { .. } => SubstitutionKind::ModuleArgument,
            Self::FnParam { .. } => SubstitutionKind::FnParam,
            Self::FnLocal { .. } => SubstitutionKind::FnLocal,
            Self::Loop(super::LoopSlot::Key) => SubstitutionKind::LoopKey,
            Self::Loop(super::LoopSlot::Index) => SubstitutionKind::LoopIndex,
            Self::Loop(super::LoopSlot::Value) => SubstitutionKind::LoopValue,
            Self::EmptyInterpolation { .. } => SubstitutionKind::EmptyInterpolation,
        }
    }
}
impl super::evaluable_leaf::Sealed for super::LoopSlot {
    fn substitution_kind(&self) -> SubstitutionKind {
        match self {
            Self::Key => SubstitutionKind::LoopKey,
            Self::Index => SubstitutionKind::LoopIndex,
            Self::Value => SubstitutionKind::LoopValue,
        }
    }
}

impl<P: PlaceholderKind, B: BindingAddress> Value<P, B> {
    /// Retain shared knowledge and protected failures across parser/template
    /// phases. Only substitution slots are erased; expression wrappers and
    /// lexical bodies keep their evidence for the ordinary classifier.
    pub(crate) fn input_preview(&self) -> Value {
        let source = match self.knowledge(&KnowledgeContext::validation()) {
            Ok(knowledge) => knowledge.to_value(),
            // Keep the source of classification failures too: a later consumer
            // must still see that failure, never a successful unknown leaf.
            Err(_) => self.clone(),
        };
        let projected = source
            .try_map_placeholder(&mut |_| {
                Ok::<_, std::convert::Infallible>(Value::Deferred(DeferredValue::Unknown(
                    ApplyTimeUnknown::PendingReRead(super::PendingReRead::not_found()),
                )))
            })
            .unwrap_or_else(|never| match never {});
        super::binding_address::input_preview(projected)
    }

    pub fn knowledge<'a, S: ClassificationStage>(
        &'a self,
        cx: &KnowledgeContext<'_, P, B, S>,
    ) -> Result<Knowledge<'a, P, B, S>, KnowledgeError<B>> {
        classify(self, cx, &mut Position::root()).map(|classification| Knowledge {
            classification,
            stage: std::marker::PhantomData,
        })
    }
}

fn reference_phase<P: PlaceholderKind, B: BindingAddress, S: ClassificationStage>(
    cx: &KnowledgeContext<'_, P, B, S>,
    binding: &B,
    selection: Option<&AccessPath<B>>,
    pos: &mut Position<'_, B>,
) -> Result<Phase, KnowledgeError<B>> {
    use crate::resolver::{PlanReferenceResolution, ReferenceError, ReferenceEvaluation};
    let bindings = match cx.inputs {
        kernel::Inputs::Validation => return Ok(Phase::Plan),
        kernel::Inputs::Plan(bindings) => bindings,
    };
    // Diagnostic rendering can redact distinct keys identically. Cycle
    // identity uses the original typed selector, independently of decoration.
    let key = match selection {
        Some(path) => WaitOn::Resource(path.clone()),
        None => WaitOn::Binding(binding.clone()),
    };
    if pos.references.contains(&key) {
        return Err(KnowledgeError::Cycle {
            operation: "reference",
        });
    }
    let mut references = pos.references.clone();
    references.push(key);
    let status =
        crate::resolver::plan_reference_status(binding, selection, bindings, |value, _| {
            let mut selected_pos = Position::root();
            selected_pos.references = references.clone();
            let proof = classify(&value, cx, &mut selected_pos)?;
            if let Some(error) = proof.failure() {
                return Err(error);
            }
            let phase = if proof.apply_required() {
                Phase::Apply
            } else {
                Phase::Plan
            };
            Ok(if matches!(proof, Classification::Known(_)) {
                ReferenceEvaluation::Known(value)
            } else {
                ReferenceEvaluation::Pending(phase)
            })
        })
        .map_err(|error| match error {
            ReferenceError::Evaluation(error) => error,
            ReferenceError::Historical(error) => KnowledgeError::UnavailableHistoricalSecret(error),
        })?;
    Ok(match status {
        PlanReferenceResolution::Present(_) => Phase::Plan,
        // Absence resolves an optional discriminator, not a required value.
        // Main retains the required reference; only the coalesce resolver may
        // consume the witness and select its fallback.
        PlanReferenceResolution::Absent(_) => Phase::Apply,
        PlanReferenceResolution::Pending(phase) => phase,
        // A residual selection miss is retained by the ordinary resolver as on
        // main. The strict status consumer owns its absence/error diagnostic.
        PlanReferenceResolution::SelectionError { .. } => Phase::Apply,
    })
}

fn pending<'a, P: PlaceholderKind, B: BindingAddress>(
    source: &'a Value<P, B>,
    wait_on: WaitOn<B>,
    phase: Phase,
    reason: PendingReason,
) -> Classification<'a, P, B> {
    Classification::Pending(Pending {
        source,
        node: kernel::PendingNode::Ordinary(Vec::new()),
        phase,
        reason,
        wait_on,
    })
}
fn expression<'a, P: PlaceholderKind, B: BindingAddress>(
    source: &'a Value<P, B>,
    kind: EvaluationKind,
    mut inputs: Vec<kernel::Input<'a, P, B>>,
) -> Classification<'a, P, B> {
    let phase = if inputs.iter().any(kernel::Input::apply_required) {
        Phase::Apply
    } else {
        Phase::Plan
    };
    // A shared lexical body is executed within this invocation. Carry the
    // invocation's timing on its edge without changing the cached analysis.
    // Ordinary value operands retain their own pending work.
    for input in &mut inputs {
        match &input.value {
            kernel::InputValue::Lexical(_) => input.apply = phase == Phase::Apply,
            kernel::InputValue::Value(_) => {}
        }
    }
    Classification::Pending(Pending {
        source,
        node: kernel::PendingNode::Ordinary(inputs),
        phase,
        reason: PendingReason::Evaluation(kind),
        wait_on: WaitOn::Evaluation(kind),
    })
}
fn classify_function<'a, P: PlaceholderKind, B: BindingAddress, S: ClassificationStage>(
    function: &'a Arc<crate::parser::UserFunction<B>>,
    cx: &KnowledgeContext<'_, P, B, S>,
    pos: &mut Position<'a, B>,
) -> Result<Arc<kernel::FunctionAnalysis<'a, B>>, KnowledgeError<B>> {
    let id = Arc::as_ptr(function) as usize;
    if pos.functions.contains(&id) {
        return Err(KnowledgeError::Cycle {
            operation: "user function",
        });
    }
    if let Some(analysis) = pos.analyses.get(&id) {
        return Ok(Arc::clone(analysis));
    }
    pos.functions.push(id);
    let previous = pos.lexical_body;
    pos.lexical_body = true;
    let result = (|| {
        let body = classify(&function.body.value, cx, pos)?;
        let locals = function
            .local_lets
            .iter()
            .map(|(_, value, _)| classify(value, cx, pos))
            .collect::<Result<Vec<_>, _>>()?;
        let apply_required =
            body.apply_required() || locals.iter().any(Classification::apply_required);
        let failure = body
            .failure()
            .or_else(|| locals.iter().find_map(Classification::failure));
        let permits_static_evaluation = failure.is_none()
            && body.permits_static_evaluation()
            && locals.iter().all(Classification::permits_static_evaluation);
        Ok(kernel::FunctionAnalysis {
            body,
            locals,
            apply_required,
            failure,
            permits_static_evaluation,
        })
    })();
    pos.lexical_body = previous;
    pos.functions.pop();
    let analysis = Arc::new(result?);
    pos.analyses.insert(id, Arc::clone(&analysis));
    Ok(analysis)
}
// Keep recursively nested interpolation out of the large variant dispatch
// frame. Classifying deep expression evidence must not consume one such frame
// per operand before the conservative estimator applies its own budget.
fn classify<
    'a,
    P: PlaceholderKind,
    Q: PlaceholderKind,
    B: BindingAddress,
    S: ClassificationStage,
>(
    value: &'a Value<Q, B>,
    cx: &KnowledgeContext<'_, P, B, S>,
    pos: &mut Position<'a, B>,
) -> Result<Classification<'a, Q, B>, KnowledgeError<B>> {
    #[cfg(test)]
    classification_step();
    match value {
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            classify_interpolation(value, parts, cx, pos)
        }
        value => classify_value(value, cx, pos),
    }
}

fn classify_interpolation<
    'a,
    P: PlaceholderKind,
    Q: PlaceholderKind,
    B: BindingAddress,
    S: ClassificationStage,
>(
    value: &'a Value<Q, B>,
    parts: &'a [InterpolationPart<Q, B>],
    cx: &KnowledgeContext<'_, P, B, S>,
    pos: &mut Position<'a, B>,
) -> Result<Classification<'a, Q, B>, KnowledgeError<B>> {
    let mut inputs = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        match part {
            InterpolationPart::Literal(_) => {}
            InterpolationPart::Expr(value) => inputs.push(kernel::Input::value(
                OperandStep::Interpolation(i),
                classify(value, cx, pos)?,
            )),
        }
    }
    Ok(expression(value, EvaluationKind::Interpolation, inputs))
}

fn classify_value<
    'a,
    P: PlaceholderKind,
    Q: PlaceholderKind,
    B: BindingAddress,
    S: ClassificationStage,
>(
    value: &'a Value<Q, B>,
    cx: &KnowledgeContext<'_, P, B, S>,
    pos: &mut Position<'a, B>,
) -> Result<Classification<'a, Q, B>, KnowledgeError<B>> {
    use kernel::{Input, InputValue, KnownNode as N};
    let known = |node| {
        Ok(Classification::Known(KnownValue {
            source: value,
            node,
        }))
    };
    match value {
        Value::Concrete(ConcreteValue::String(v)) => known(N::String(v)),
        Value::Concrete(ConcreteValue::EnumIdentifier(v)) => known(N::EnumIdentifier(v)),
        Value::Concrete(ConcreteValue::CanonicalEnum(v)) => known(N::CanonicalEnum(v)),
        Value::Concrete(ConcreteValue::Int(v)) => known(N::Int(*v)),
        Value::Concrete(ConcreteValue::Float(v)) => known(N::Float(*v)),
        Value::Concrete(ConcreteValue::Bool(v)) => known(N::Bool(*v)),
        Value::Concrete(ConcreteValue::Duration(v)) => known(N::Duration(*v)),
        Value::Concrete(ConcreteValue::StringList(v)) => known(N::StringList(v)),
        Value::Concrete(ConcreteValue::List(values)) => {
            let mut known_items = Vec::with_capacity(values.len());
            let mut partial: Option<Vec<Classification<'a, Q, B>>> = None;
            for child in values {
                let child = classify(child, cx, pos)?;
                if let Some(items) = &mut partial {
                    items.push(child);
                } else {
                    match child {
                        Classification::Known(value) => known_items.push(value),
                        child @ (Classification::ShapeKnown(_)
                        | Classification::Pending(_)
                        | Classification::Failed(_)) => {
                            let mut items = std::mem::take(&mut known_items)
                                .into_iter()
                                .map(Classification::Known)
                                .collect::<Vec<_>>();
                            items.push(child);
                            partial = Some(items);
                        }
                    }
                }
            }
            match partial {
                None => known(N::List(KnownList { items: known_items })),
                Some(children) => {
                    let apply_required = children.iter().any(Classification::apply_required);
                    Ok(Classification::ShapeKnown(ShapeKnown {
                        source: value,
                        node: kernel::ShapeNode::List(children),
                        apply_required,
                    }))
                }
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            let mut known_entries = IndexMap::with_capacity(values.len());
            let mut partial: Option<IndexMap<&'a str, Classification<'a, Q, B>>> = None;
            for (key, child) in values {
                let child = classify(child, cx, pos)?;
                if let Some(entries) = &mut partial {
                    entries.insert(key.as_str(), child);
                } else {
                    match child {
                        Classification::Known(value) => {
                            known_entries.insert(key.as_str(), value);
                        }
                        child @ (Classification::ShapeKnown(_)
                        | Classification::Pending(_)
                        | Classification::Failed(_)) => {
                            let mut entries = std::mem::take(&mut known_entries)
                                .into_iter()
                                .map(|(key, value)| (key, Classification::Known(value)))
                                .collect::<IndexMap<_, _>>();
                            entries.insert(key.as_str(), child);
                            partial = Some(entries);
                        }
                    }
                }
            }
            match partial {
                None => known(N::Map(KnownMap {
                    entries: known_entries,
                })),
                Some(children) => {
                    let apply_required = children.values().any(Classification::apply_required);
                    Ok(Classification::ShapeKnown(ShapeKnown {
                        source: value,
                        node: kernel::ShapeNode::Map(children),
                        apply_required,
                    }))
                }
            }
        }
        Value::Deferred(DeferredValue::Secret(payload)) => {
            let SecretClassification(inner) =
                payload.try_inspect(|inner| classify(inner, cx, pos).map(SecretClassification))?;
            if let Some(error) = inner.failure() {
                return Ok(Classification::Failed(AttemptedFailure {
                    source: value,
                    error: error.inside_secret(),
                }));
            }
            match inner {
                Classification::Known(_) => known(N::Secret(ProtectedSecret { payload })),
                inner @ (Classification::ShapeKnown(_) | Classification::Pending(_)) => {
                    let apply = inner.apply_required();
                    let site = inner.sites().next().ok_or(KnowledgeError::InvalidStage {
                        operation: "protected pending value without a site",
                    })?;
                    Ok(Classification::Pending(Pending {
                        source: value,
                        phase: if apply { Phase::Apply } else { Phase::Plan },
                        reason: site.reason,
                        wait_on: site.wait_on,
                        node: kernel::PendingNode::Secret {
                            protected: ProtectedSecret { payload },
                            inner: Box::new(inner),
                        },
                    }))
                }
                Classification::Failed(failure) => Ok(Classification::Failed(AttemptedFailure {
                    source: value,
                    error: failure.error().inside_secret(),
                })),
            }
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => Ok(pending(
            value,
            WaitOn::Resource(path.clone()),
            reference_phase(cx, path.binding_address(), Some(path), pos)?,
            PendingReason::ResourceSelection,
        )),
        Value::Deferred(DeferredValue::BindingRef { binding }) => Ok(pending(
            value,
            WaitOn::Binding(binding.clone()),
            reference_phase(cx, binding, None, pos)?,
            PendingReason::BindingSelection,
        )),
        Value::Deferred(DeferredValue::Unknown(unknown)) => {
            let reason = match unknown {
                ApplyTimeUnknown::UpstreamExport { .. } => PendingReason::UpstreamExport,
                ApplyTimeUnknown::UpstreamAllExports { .. } => PendingReason::UpstreamAllExports,
                ApplyTimeUnknown::PendingReRead(_) => PendingReason::ReadBack,
            };
            Ok(pending(
                value,
                WaitOn::Unknown(unknown.clone()),
                Phase::Apply,
                reason,
            ))
        }
        Value::Deferred(DeferredValue::Placeholder(slot)) => {
            let kind = slot.substitution_kind();
            if pos.lexical_body
                && matches!(kind, SubstitutionKind::FnParam | SubstitutionKind::FnLocal)
            {
                return Ok(expression(value, EvaluationKind::UserFunction, Vec::new()));
            }
            Ok(pending(
                value,
                WaitOn::Substitution(kind),
                Phase::Plan,
                kind.reason(),
            ))
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            reason,
            args: _,
            source_line,
        }) => Ok(Classification::Failed(AttemptedFailure {
            source: value,
            error: KnowledgeError::Unavailable {
                name: name.to_string(),
                reason: SecretDiagnosticError::Public(reason.clone()),
                source_line: *source_line,
            },
        })),
        Value::Deferred(DeferredValue::IntegerArithmetic(expr)) => Ok(expression(
            value,
            EvaluationKind::IntegerArithmetic,
            vec![
                Input::value(OperandStep::ArithmeticLeft, classify(expr.lhs(), cx, pos)?),
                Input::value(OperandStep::ArithmeticRight, classify(expr.rhs(), cx, pos)?),
            ],
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            classify_interpolation(value, parts, cx, pos)
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let input = match access.base().as_concrete() {
                Some(root) => match super::select_concrete_member(root, access.step()) {
                    Ok(super::ConcreteSelection::Member {
                        value: selected,
                        step,
                    }) => {
                        let mut input =
                            Input::value(OperandStep::AccessBase, classify(selected, cx, pos)?);
                        input.selection = Some(step);
                        Some(input)
                    }
                    // String-list members are already concrete scalars. Misses
                    // and wrong shapes retain their authored Access diagnostic.
                    Ok(super::ConcreteSelection::String(_)) | Err(_) => None,
                },
                None => Some(Input::value(
                    OperandStep::AccessBase,
                    classify(access.base(), cx, pos)?,
                )),
            };
            Ok(expression(
                value,
                EvaluationKind::Access,
                input.into_iter().collect(),
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line: _,
        }) => {
            let mut inputs = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                if arg.value().is_some() || arg.pending_parameter().is_some() {
                    let argument = classify(arg.walker_value(), cx, pos)?;
                    use crate::builtins::BuiltinFunction as F;
                    let shape_only = match name {
                        F::length | F::keys | F::values | F::concat => true,
                        F::lookup => i == 0,
                        F::map => i == 1,
                        F::flatten => match &argument {
                            Classification::Known(_) => true,
                            Classification::ShapeKnown(value) => value.visit(
                                |view| match view {
                                    ShapeView::List(items) => items.iter().all(|item| {
                                        item.flatten_item().is_ok_and(|item| item.is_some())
                                    }),
                                    ShapeView::Map(_) => true,
                                },
                                |_| true,
                            ),
                            Classification::Pending(_) | Classification::Failed(_) => false,
                        },
                        F::cidr_subnet
                        | F::decrypt
                        | F::env
                        | F::join
                        | F::max
                        | F::min
                        | F::replace
                        | F::split
                        | F::trim
                        | F::lower
                        | F::upper => false,
                    };
                    let visible = !shape_only
                        || match &argument {
                            Classification::Known(_) => false,
                            Classification::Pending(_) => true,
                            Classification::ShapeKnown(_) | Classification::Failed(_) => false,
                        };
                    let mut input = Input::value(OperandStep::Argument(i), argument);
                    input.visible = visible;
                    inputs.push(input);
                }
            }
            Ok(expression(value, EvaluationKind::Builtin, inputs))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: _,
            args,
            source_line: _,
        }) => {
            let mut inputs = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                if arg.value().is_some() || arg.pending_parameter().is_some() {
                    inputs.push(Input::value(
                        OperandStep::Argument(i),
                        classify(arg.walker_value(), cx, pos)?,
                    ));
                }
            }
            let analysis = classify_function(function, cx, pos)?;
            inputs.push(Input {
                step: None,
                selection: None,
                value: InputValue::Lexical(analysis),
                visible: true,
                guard: None,
                apply: false,
            });
            Ok(expression(value, EvaluationKind::UserFunction, inputs))
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: left_source,
            default,
        }) => {
            let left = classify(left_source, cx, pos)?;
            let default = classify(default, cx, pos)?;
            let apply = left.apply_required();
            let shape = match left {
                Classification::ShapeKnown(shape) => Ok(shape),
                Classification::Known(mut known) => match &mut known.node {
                    N::List(items) => Ok(ShapeKnown {
                        source: left_source,
                        node: kernel::ShapeNode::List(
                            std::mem::take(&mut items.items)
                                .into_iter()
                                .map(Classification::Known)
                                .collect(),
                        ),
                        apply_required: apply,
                    }),
                    N::Map(items) => Ok(ShapeKnown {
                        source: left_source,
                        node: kernel::ShapeNode::Map(
                            std::mem::take(&mut items.entries)
                                .into_iter()
                                .map(|(k, v)| (k, Classification::Known(v)))
                                .collect(),
                        ),
                        apply_required: apply,
                    }),
                    N::String(_)
                    | N::EnumIdentifier(_)
                    | N::CanonicalEnum(_)
                    | N::Int(_)
                    | N::Float(_)
                    | N::Bool(_)
                    | N::Duration(_)
                    | N::StringList(_)
                    | N::Secret(_) => Err(Classification::Known(known)),
                },
                other @ (Classification::Pending(_) | Classification::Failed(_)) => Err(other),
            };
            let alternative = kernel::CoalesceAlternative {
                evidence: Box::new(default),
            };
            match shape {
                Ok(shape) => Ok(Classification::ShapeKnown(ShapeKnown {
                    source: value,
                    node: kernel::ShapeNode::Coalesce(kernel::CoalescePending {
                        value: Box::new(shape),
                        alternative,
                    }),
                    apply_required: apply,
                })),
                Err(left) => Ok(Classification::Pending(Pending {
                    source: value,
                    node: kernel::PendingNode::Coalesce(kernel::CoalescePending {
                        value: Box::new(left),
                        alternative,
                    }),
                    phase: if apply { Phase::Apply } else { Phase::Plan },
                    reason: PendingReason::Evaluation(EvaluationKind::Coalesce),
                    wait_on: WaitOn::Evaluation(EvaluationKind::Coalesce),
                })),
            }
        }
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let [condition, then_value, else_value] = deferred.values();
            let condition = classify(condition, cx, pos)?;
            let apply = condition.apply_required();
            let mut inputs = vec![Input::value(OperandStep::Condition, condition)];
            for (step, branch) in [
                (OperandStep::Then, then_value),
                (OperandStep::Else, else_value),
            ] {
                let mut input = Input::value(step, classify(branch, cx, pos)?);
                input.guard = Some(PendingGuard::ConditionalBranch);
                input.apply = apply;
                inputs.push(input);
            }
            Ok(expression(value, EvaluationKind::ModuleIf, inputs))
        }
    }
}
/// Definitive absence of an eligible selection. Only the existing resolution
/// and selection boundaries can mint this evidence.
///
/// ```compile_fail,E0308
/// use carina_core::resource::{Value, knowledge::take_fallback};
/// fn bypass(raw: Value) { let _ = take_fallback(raw, || Ok::<_, ()>(())); }
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct AbsenceWitness<B: BindingAddress = AbsoluteBindingAddress> {
    inner: kernel_absence::Absence<B>,
}
mod kernel_absence {
    #[derive(Clone, PartialEq, Eq)]
    pub(super) struct Absence<B: super::BindingAddress>(pub(super) std::marker::PhantomData<B>);
}
impl<B: BindingAddress> fmt::Debug for AbsenceWitness<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AbsenceWitness")
    }
}
impl<B: BindingAddress> AbsenceWitness<B> {
    pub(crate) fn binding_miss<P: PlaceholderKind>(
        path: &AccessPath<B>,
        bindings: &ResolvedBindings<P, B>,
    ) -> Option<Self> {
        (bindings.attribute_absence_is_definitive(path.binding(), path.attribute())
            && bindings
                .get(path.binding())
                .is_some_and(|attrs| !attrs.contains_key(path.attribute())))
        .then_some(Self {
            inner: kernel_absence::Absence(std::marker::PhantomData),
        })
    }
    pub(crate) fn selection_miss(error: &super::ValueAccessError) -> Option<Self> {
        match error {
            super::ValueAccessError::MissingField { .. }
            | super::ValueAccessError::MissingKey { .. } => Some(Self {
                inner: kernel_absence::Absence(std::marker::PhantomData),
            }),
            super::ValueAccessError::IndexOutOfRange { .. }
            | super::ValueAccessError::WrongShape { .. }
            | super::ValueAccessError::Classification { .. } => None,
        }
    }
    pub(crate) fn in_domain<C: BindingAddress>(self) -> AbsenceWitness<C> {
        AbsenceWitness {
            inner: kernel_absence::Absence(std::marker::PhantomData),
        }
    }
}
/// Reconstruct an already-proven present value.
///
/// ```compile_fail,E0308
/// use carina_core::resource::{Value, knowledge::take_present};
/// fn bypass(raw: Value) { let _ = take_present(raw); }
/// ```
pub fn take_present<P: PlaceholderKind, B: BindingAddress>(
    value: KnownValue<'_, P, B>,
) -> Value<P, B> {
    value.to_value()
}

/// Select a fallback only after a definitive eligible miss.
///
/// ```compile_fail,E0308
/// use carina_core::resource::{Value, knowledge::take_fallback};
/// fn bypass(raw: Value) { let _ = take_fallback(raw, || Ok::<_, ()>(())); }
/// ```
pub fn take_fallback<B: BindingAddress, R, E>(
    _absence: AbsenceWitness<B>,
    fallback: impl FnOnce() -> Result<R, E>,
) -> Result<R, E> {
    fallback()
}

/// Syntax eligibility for absence propagation. This descriptor grants no
/// absence or knowledge of the operand's value.
pub struct OptionalOperand<'a, P: super::ValueKind, B: BindingAddress> {
    value: &'a Value<P, B>,
}
impl<'a, P: super::ValueKind, B: BindingAddress> OptionalOperand<'a, P, B> {
    pub(crate) fn classify(value: &'a Value<P, B>) -> Option<Self> {
        fn reference<P: super::ValueKind, B: BindingAddress>(value: &Value<P, B>) -> bool {
            match value {
                Value::Deferred(
                    DeferredValue::ResourceRef { .. } | DeferredValue::BindingRef { .. },
                ) => true,
                Value::Deferred(DeferredValue::Access(access)) => reference(access.base()),
                _ => false,
            }
        }
        let eligible = match value {
            Value::Deferred(DeferredValue::Coalesce { .. }) => true,
            _ => reference(value),
        };
        eligible.then_some(Self { value })
    }
    pub(crate) fn value(&self) -> &Value<P, B> {
        self.value
    }
}

impl<P: PlaceholderKind, B: BindingAddress> ProtectedSecret<'_, P, B> {
    pub(crate) fn permits_import_identifier(&self) -> bool {
        self.payload.visit(|value| {
            matches!(
                value,
                Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_))
            )
        })
    }
}
impl KnownValue<'_> {
    /// Named provider-import projection. It never exposes an ordinary view of
    /// a Secret and produces only the existing provider transport identifier.
    pub(crate) fn project_applied_import(
        &self,
    ) -> Result<crate::provider::AppliedIdentifier, crate::provider::EmptyAppliedIdentifier> {
        use kernel::KnownNode as N;
        match &self.node {
            N::String(value) => crate::provider::AppliedIdentifier::new(value.expose_plaintext()),
            N::EnumIdentifier(value) => crate::provider::AppliedIdentifier::new(value.as_str()),
            N::Secret(secret) => secret.payload.try_project_plaintext(|value| match value {
                Value::Concrete(ConcreteValue::String(value)) => {
                    crate::provider::AppliedIdentifier::new(value.expose_plaintext())
                }
                Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
                    crate::provider::AppliedIdentifier::new(value.as_str())
                }
                _ => Err(crate::provider::EmptyAppliedIdentifier),
            }),
            N::CanonicalEnum(_)
            | N::Int(_)
            | N::Float(_)
            | N::Bool(_)
            | N::Duration(_)
            | N::List(_)
            | N::StringList(_)
            | N::Map(_) => Err(crate::provider::EmptyAppliedIdentifier),
        }
    }
}
impl InsideSecret for crate::provider::EmptyAppliedIdentifier {
    fn inside_secret(self) -> Self {
        self
    }
}

/// The Bool consumer rejects protected and non-Bool values with its owner's diagnostic.
pub(crate) fn known_bool<P: PlaceholderKind, B: BindingAddress>(
    value: &KnownValue<'_, P, B>,
) -> Option<bool> {
    value.visit(
        |value| match value {
            KnownView::Bool(value) => Some(value),
            _ => None,
        },
        |_| None,
    )
}

#[cfg(test)]
mod classification_contract {
    use super::*;
    use crate::resource::{PendingReRead, RelativeBindingName};
    use std::convert::Infallible;
    type V = Value<NoPlaceholder, RelativeBindingName>;

    #[test]
    fn every_concrete_axis_is_known_without_granting_wire_or_enum_authority() {
        use crate::{
            resource::{EnumValueResolver, RawEnumIdentifier},
            schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity},
        };
        let schema = ResourceSchema::new("test.Widget")
            .attribute(
                AttributeSchema::optional(
                    "mode",
                    AttributeType::enum_(
                        TypeIdentity::new(Some("test"), ["widget"], "Mode"),
                        Some(vec!["Enabled".into()]),
                        vec![],
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let canonical = EnumValueResolver::new(schema.as_schema().attributes()["mode"].attr_type())
            .resolve_raw(&RawEnumIdentifier::parse("test.widget.Mode.Enabled"))
            .unwrap();
        let values: Vec<V> = vec![
            Value::Concrete(ConcreteValue::String("value".into())),
            Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.Enabled")),
            Value::Concrete(ConcreteValue::CanonicalEnum(canonical)),
            Value::Concrete(ConcreteValue::Int(4)),
            Value::Concrete(ConcreteValue::Float(crate::finite(1.25))),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(1))),
            Value::Concrete(ConcreteValue::StringList(vec!["item".into()])),
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        ];
        for value in values {
            assert!(matches!(
                value
                    .knowledge(&KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
                Classification::Known(_)
            ));
        }
    }

    #[test]
    fn every_unknown_reason_and_source_free_expression_keeps_its_typed_wait() {
        use crate::{
            builtins::BuiltinFunction,
            integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values},
            resource::{AccessMissPolicy, AccessPath, PathSegment},
        };
        let unknowns: Vec<V> = vec![
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: AccessPath::new("up", "items"),
            })),
            Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports {
                    binding: RelativeBindingName::new("up"),
                },
            )),
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
                PendingReRead::not_found(),
            ))),
        ];
        for value in &unknowns {
            let Classification::Pending(value) = value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
            else {
                panic!("unknown")
            };
            assert_eq!(value.phase(), Phase::Apply);
            assert_eq!(value.sites().count(), 1);
        }
        let access = crate::resource::apply_postfix_access(
            unknowns[0].clone(),
            PathSegment::Field {
                name: "nested".into(),
            },
            0,
            AccessMissPolicy::Error,
        )
        .unwrap();
        let arithmetic = evaluate_integer_values(
            V::resource_ref("producer", "count", vec![]),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap();
        let builtin: V = Value::Deferred(DeferredValue::FunctionCall {
            name: BuiltinFunction::upper,
            args: vec![Value::Concrete(ConcreteValue::String("text".into())).into()],
            source_line: 0.into(),
        });
        for (value, kind) in [
            (access, EvaluationKind::Access),
            (arithmetic, EvaluationKind::IntegerArithmetic),
            (builtin, EvaluationKind::Builtin),
        ] {
            let Classification::Pending(value) = value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
            else {
                panic!("operation")
            };
            assert!(value.sites().any(|site| {
                site.inspect::<bool, Infallible>(|_, _, wait| {
                    Ok(matches!(wait,WaitOn::Evaluation(actual) if *actual==kind))
                })
                .unwrap()
            }));
        }
        let failure: V = Value::Deferred(DeferredValue::Unavailable {
            name: BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error("attempted".into()),
            source_line: 0.into(),
        });
        assert!(matches!(
            PlanResolved::try_from(
                failure
                    .knowledge(&KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(KnowledgeError::Unavailable { .. })
        ));
        let parsed = crate::parser::parse_file(
            "arguments { flag: Bool }\nlet choice = if flag { 1 } else { 2 }\n",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let Classification::Pending(choice) = parsed.variables["choice"]
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap()
        else {
            panic!("conditional")
        };
        assert_eq!(choice.phase(), Phase::Plan);
        assert!(choice.sites().any(|site| {
            site.inspect::<bool, Infallible>(|_, _, wait| {
                Ok(matches!(wait, WaitOn::Evaluation(EvaluationKind::ModuleIf)))
            })
            .unwrap()
        }));
        for (json, kind) in [
            (
                r#"{"ModuleArgument":{"name":"input"}}"#,
                SubstitutionKind::ModuleArgument,
            ),
            (
                r#"{"EmptyInterpolation":{}}"#,
                SubstitutionKind::EmptyInterpolation,
            ),
        ] {
            let value: Value<ParserPlaceholder, RelativeBindingName> = Value::Deferred(
                DeferredValue::Placeholder(serde_json::from_str(json).unwrap()),
            );
            let Classification::Pending(value) = value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
            else {
                panic!("substitution")
            };
            assert!(
                value
                    .sites()
                    .next()
                    .unwrap()
                    .inspect::<bool, Infallible>(|_, _, wait| Ok(
                        matches!(wait,WaitOn::Substitution(actual) if *actual==kind)
                    ))
                    .unwrap()
            );
        }
    }

    #[test]
    fn plan_reference_phase_uses_real_managed_stability_and_wait_alias_facts() {
        use crate::{
            binding_index::{
                LivePreApplyInputs, ManagedBindingStability, ResolvedBindings, WaitAliasSpec,
            },
            data_source_value::DataSourceValues,
            resource::{Current, Resource, RootScope, State},
            schema::SchemaRegistry,
        };
        use std::collections::{HashMap, HashSet};
        let scope = RootScope::new();
        let producer = scope.resolve(RelativeBindingName::new("producer"));
        let managed: Vec<Resource> = vec![
            Resource::with_provider("test", "Widget", "producer", None)
                .with_absolute_binding(producer.clone())
                .with_attribute(
                    "name",
                    Value::Concrete(ConcreteValue::String("authored".into())),
                ),
        ];
        let existing = HashMap::from([(
            managed[0].id.clone(),
            Current::from_state(
                State::existing(managed[0].id.clone(), HashMap::new()).with_identifier("id"),
            ),
        )]);
        let existing = crate::resource::into_plan_input_current_map(
            existing,
            &SchemaRegistry::new(),
            &managed,
        );
        let unavailable = ManagedBindingStability::Unavailable;
        let updating = ManagedBindingStability::Updating {
            changed_attributes: HashSet::new(),
            refreshable_attributes: HashSet::from(["output".into()]),
        };
        let removed = ManagedBindingStability::Updating {
            changed_attributes: HashSet::from(["output".into()]),
            refreshable_attributes: HashSet::new(),
        };
        for (present, stability, _expected) in [
            (
                false,
                ManagedBindingStability::NoPlannedEffect,
                Phase::Apply,
            ),
            (true, ManagedBindingStability::NoPlannedEffect, Phase::Apply),
            (true, unavailable, Phase::Apply),
            (true, updating, Phase::Apply),
            (true, removed, Phase::Apply),
        ] {
            let empty = HashMap::new();
            let bindings = ResolvedBindings::pre_apply_live_with_stability(
                LivePreApplyInputs {
                    managed: &managed,
                    compositions: &[],
                    managed_current: if present { &existing } else { &empty },
                    data_source_values: &DataSourceValues::empty(),
                    remote_bindings: &HashMap::new(),
                    wait_aliases: &[WaitAliasSpec {
                        binding: scope.resolve(RelativeBindingName::new("ready")),
                        target: producer.clone(),
                    }],
                },
                &HashMap::from([(managed[0].id.clone(), stability)]),
            );
            for name in ["producer", "ready"] {
                let value = Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::from_binding(
                        scope.resolve(RelativeBindingName::new(name)),
                        "output",
                        vec![],
                    ),
                });
                let Classification::Pending(pending) = value
                    .knowledge(&KnowledgeContext::plan(&bindings))
                    .map(|proof| proof.into_classification())
                    .unwrap()
                else {
                    panic!("selection")
                };
                let status = crate::resolver::project_ref_value_status(
                    &value,
                    &bindings,
                    &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                let expected = match status {
                    crate::resolver::ValueResolution::Present(_) => Phase::Plan,
                    crate::resolver::ValueResolution::Absent(_)
                    | crate::resolver::ValueResolution::Unknown(_) => Phase::Apply,
                };
                assert_eq!(pending.phase(), expected, "{name}, present={present}");
            }
            let authored = Value::Deferred(DeferredValue::ResourceRef {
                path: crate::resource::AccessPath::from_binding(producer.clone(), "name", vec![]),
            });
            let Classification::Pending(pending) = authored
                .knowledge(&KnowledgeContext::plan(&bindings))
                .map(|proof| proof.into_classification())
                .unwrap()
            else {
                panic!("authored reference still needs selection")
            };
            assert_eq!(pending.phase(), Phase::Plan);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod selection_tests {
    use super::*;
    #[test]
    fn concrete_access_allocates_no_knowledge_nodes_for_unselected_members() {
        for size in [16, 65_536] {
            for index in 0..8 {
                let base: Value = Value::Concrete(ConcreteValue::List(
                    (0..size)
                        .map(|_| Value::Concrete(ConcreteValue::Int(7)))
                        .collect(),
                ));
                with_classification_budget(0, || {
                    let selected = super::super::apply_value_path_segment(
                        base,
                        super::super::PathSegment::Subscript {
                            index: super::super::Subscript::Int { index },
                        },
                        0,
                        super::super::AccessMissPolicy::Error,
                    )
                    .unwrap();
                    assert!(matches!(
                        selected,
                        super::super::ValuePathSegmentResult::Resolved(Value::Concrete(
                            ConcreteValue::Int(7)
                        ))
                    ));
                });
            }
        }
    }
    #[test]
    fn sensitive_selection_site_never_displays_key_plaintext() {
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::new([(
            "CARINA_KNOWLEDGE_SELECTION_KEY".to_owned(),
            "PRIVATE_SELECTION_ENV_KEY".into(),
        )]));
        let key_value: Value = crate::builtins::evaluate_builtin_with_config_to_value(
            "env",
            &[Value::Concrete(ConcreteValue::String(
                "CARINA_KNOWLEDGE_SELECTION_KEY".into(),
            ))],
            &context,
        )
        .unwrap();
        let Value::Concrete(ConcreteValue::String(key)) = key_value else {
            panic!("env string")
        };
        let base = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            key.expose_plaintext().to_owned(),
            Value::root_resource_ref("future", "identifier", vec![]),
        )])));
        let access = super::super::deferred_access(
            base,
            super::super::PathSegment::Subscript {
                index: super::super::Subscript::Str { key },
            },
            0,
        );
        let bindings = ResolvedBindings::default();
        let proof = access
            .knowledge(&KnowledgeContext::plan(&bindings))
            .map(|proof| proof.into_classification())
            .unwrap();
        let site = proof
            .sites()
            .find(|site| !site.path.segments().is_empty())
            .unwrap();
        assert!(!site.path.to_string().contains("PRIVATE_SELECTION_ENV_KEY"));
        assert!(!format!("{:?}", site.path).contains("PRIVATE_SELECTION_ENV_KEY"));
        let violation = PhaseViolation {
            required: Requirement::ShapeForPlan,
            site: Box::new(site),
            source: None,
        };
        assert!(
            !violation.to_string().contains("PRIVATE_SELECTION_ENV_KEY"),
            "{violation}"
        );
        assert!(
            violation.to_string().contains("<sensitive string>"),
            "{violation}"
        );
    }
}

pub(crate) fn join_known_values<B: BindingAddress>(
    args: &[KnownValue<'_, NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, ResolutionError> {
    if args.len() != 2 {
        return Err(format!(
            "join() expects 2 arguments (separator, list), got {}",
            args.len()
        )
        .into());
    }
    let separator = args[0]
        .visit(
            |value| match value {
                KnownView::String(value) => Some(value.clone()),
                _ => None,
            },
            |_| None,
        )
        .ok_or_else(|| {
            format!(
                "join() first argument must be a string, got {}",
                crate::builtins::value_type_name(&args[0].to_value())
            )
        })?;
    let rendered = args[1].visit(
        |value| match value {
            KnownView::List(items) => items.iter().map(join_scalar).collect::<Result<Vec<_>, _>>(),
            _ => Err(format!(
                "join() second argument must be a list, got {}",
                crate::builtins::value_type_name(&args[1].to_value())
            )
            .into()),
        },
        |_| {
            Err(format!(
                "join() second argument must be a list, got {}",
                crate::builtins::value_type_name(&args[1].to_value())
            )
            .into())
        },
    )?;
    let contains_secret = rendered.iter().any(|(_, secret)| *secret);
    let text = rendered
        .iter()
        .map(|(text, _)| text.expose_plaintext())
        .collect::<Vec<_>>()
        .join(separator.expose_plaintext());
    let text = ValueString::derived_from(
        text,
        std::iter::once(&separator).chain(rendered.iter().map(|(text, _)| text)),
    );
    let value = Value::Concrete(ConcreteValue::String(text));
    Ok(if contains_secret {
        Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
    } else {
        value
    })
}
fn join_scalar<B: BindingAddress>(
    value: &KnownValue<'_, NoPlaceholder, B>,
) -> Result<(ValueString, bool), ResolutionError> {
    value.visit(
        |view| match view {
            KnownView::String(value) => Ok((value.clone(), false)),
            KnownView::EnumIdentifier(value) => Ok((value.as_str().into(), false)),
            KnownView::CanonicalEnum(value) => Ok((value.api_value().into(), false)),
            KnownView::Int(value) => Ok((value.to_string().into(), false)),
            KnownView::Float(value) => Ok((value.to_string().into(), false)),
            KnownView::Bool(value) => Ok((value.to_string().into(), false)),
            KnownView::Duration(value) => Ok((crate::value::render_duration(value).into(), false)),
            KnownView::List(_) | KnownView::Map(_) | KnownView::StringList(_) => Err(format!(
                "join() list elements must be scalar, got {}",
                crate::builtins::value_type_name(&value.to_value())
            )
            .into()),
        },
        |secret| {
            let (text, _) = secret.payload.try_project_plaintext(|value| {
                let known = value
                    .knowledge(&KnowledgeContext::validation())
                    .map_err(ResolutionError::from)?
                    .into_classification()
                    .into_known()
                    .map_err(|_| "join() requires known elements".to_owned())?;
                join_scalar(&known)
            })?;
            Ok((text, true))
        },
    )
}
