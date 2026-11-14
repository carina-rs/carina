//! The existing assignment relation and value validator make one decision.

use super::*;
use crate::binding_index::RefType;
use crate::resource::{DeferredValue, ReferencePath};
use crate::schema::assignability::AssignmentRelation;

#[test]
fn reference_judgement_uses_the_contract_already_found() {
    use crate::schema::{AttributeType, TypeIdentity};
    let string = |max| {
        AttributeType::string_member(
            crate::schema::StringIdentityLayer::new(
                Some(TypeIdentity::bare("Id")),
                None,
                None,
                None,
            ),
            crate::schema::StringMemberConstraints::new(None, Some((None, Some(max)))),
        )
    };
    super::tests::fixture(
        string(64),
        string(36),
        |catalog, _, bindings, destination| {
            crate::validation::deferred_types::with_session_for_test(catalog, |session| {
                let value = Value::root_resource_ref("producer", "output", vec![]);
                let (_, context) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(bindings),
                    session,
                );
                let CandidateOrigin::Reference(reference) = CandidateOrigin::of(&value) else {
                    panic!("reference origin")
                };
                let contract = destination.contract.as_deref().unwrap();
                let mut findings = crate::schema::value_validation::AssignmentFindings::default();
                reference
                    .judge_at::<kind::ResourceAttribute>(
                        contract,
                        contract.attr(),
                        &contract.path,
                        &ReferenceContext::from(&context),
                        &mut findings,
                    )
                    .unwrap();
                assert_eq!(
                    findings
                        .local
                        .into_iter()
                        .map(LocalPath::into_path)
                        .collect::<Vec<_>>(),
                    vec![contract.path.clone()]
                );
            });
        },
    );
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentProof<K: AssignmentKind> {
    destination: Destination<K>,
}

/// Only a kind that rechecks resolved values can mint a local-path token.
/// The shared value traversal may collect and protect tokens, but cannot
/// manufacture one from a path or a strict destination kind.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalPath(LeafPath);

impl LocalPath {
    pub(super) fn for_recheck<K: RechecksResolved>(_: K, path: LeafPath) -> Self {
        Self(path)
    }

    pub(super) fn into_path(self) -> LeafPath {
        self.0
    }
}

impl crate::resource::InsideSecret for LocalPath {
    fn inside_secret(self) -> Self {
        Self(self.0.inside_secret())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaObligation {
    requirements: Vec<SchemaRequirement>,
}
#[derive(Debug, Clone, PartialEq)]
enum SchemaRequirement {
    Type(crate::parser::TypeExpr),
    Reference,
    Destination,
}
impl SchemaObligation {
    pub(super) fn require_reference(&mut self) {
        self.requirements.push(SchemaRequirement::Reference);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.requirements.extend(other.requirements);
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.requirements.is_empty()
    }
    fn destination() -> Self {
        Self {
            requirements: vec![SchemaRequirement::Destination],
        }
    }

    fn has_unavailable_type(&self, catalog: &ProviderContext) -> bool {
        fn unavailable(ty: &crate::parser::TypeExpr, catalog: &ProviderContext) -> bool {
            use crate::parser::TypeExpr;
            match ty {
                TypeExpr::UnloadedSchemaType(unloaded) => catalog
                    .custom_types
                    .may_defer(unloaded.identity().provider.as_deref()),
                TypeExpr::SchemaType { owned } => owned
                    .provider_names()
                    .any(|owner| catalog.custom_types.may_defer(Some(owner))),
                TypeExpr::Ref(path) | TypeExpr::DottedUnresolved(path) => {
                    catalog.custom_types.may_defer(Some(&path.provider))
                }
                TypeExpr::List(inner) | TypeExpr::Map(inner) => unavailable(inner, catalog),
                TypeExpr::Union(members) => members.iter().any(|ty| unavailable(ty, catalog)),
                TypeExpr::Struct { fields } => {
                    fields.iter().any(|(_, ty)| unavailable(ty, catalog))
                }
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::StringLiteral(_) => false,
            }
        }
        self.requirements
            .iter()
            .any(|requirement| match requirement {
                SchemaRequirement::Type(ty) => unavailable(ty, catalog),
                // A KnownValue and the freshly checked destination contract supply
                // the authority that these validation-stage obligations lacked.
                SchemaRequirement::Reference | SchemaRequirement::Destination => false,
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum AssignmentVerdict<K: AssignmentKind> {
    Proven(AssignmentProof<K>),
    NeedsLocal(K::LocalObligation),
    NeedsSchema(SchemaObligation),
}

/// `of` exhaustively classifies the actual value. A judged type alone cannot
/// select reference origin, including for a future conversion.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum CandidateOrigin<B: BindingAddress> {
    Reference(ReferenceCandidate<B>),
    Expression,
}
impl<B: BindingAddress> CandidateOrigin<B> {
    pub(super) fn of<P: PlaceholderKind>(value: &Value<P, B>) -> Self {
        match value {
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                Self::Reference(ReferenceCandidate {
                    reference: ReferencePath::Access(path.clone()),
                })
            }
            Value::Deferred(DeferredValue::BindingRef { binding }) => {
                Self::Reference(ReferenceCandidate {
                    reference: ReferencePath::Binding(binding.clone()),
                })
            }
            Value::Concrete(_)
            | Value::Deferred(
                DeferredValue::ModuleIf(_)
                | DeferredValue::Placeholder(_)
                | DeferredValue::Coalesce { .. }
                | DeferredValue::IntegerArithmetic(_)
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Access(_)
                | DeferredValue::Secret(_)
                | DeferredValue::Unknown(_),
            ) => Self::Expression,
        }
    }
}

/// A typed reference origin; there is no public constructor from a raw value.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceCandidate<B: BindingAddress = AbsoluteBindingAddress> {
    pub(super) reference: ReferencePath<B>,
}

/// Reference checks borrow boundary authority without copying a value proof.
pub(crate) struct ReferenceContext<'a, B: BindingAddress> {
    pub(super) bindings: Option<&'a BindingIndex<'a, B>>,
    pub(super) provider: &'a dyn ProviderTypeCheckContext,
    pub(super) declaration: Option<(&'a str, &'a crate::binding_index::DeclaredValueSource)>,
}
impl<'a, P: PlaceholderKind, B: BindingAddress> From<&AdmissionContext<'a, P, B>>
    for ReferenceContext<'a, B>
{
    fn from(context: &AdmissionContext<'a, P, B>) -> Self {
        Self {
            bindings: context.bindings,
            provider: context.provider,
            declaration: context.declaration,
        }
    }
}

impl<B: BindingAddress> ReferenceCandidate<B> {
    pub(super) fn error_with_origin(&self, error: TypeError) -> TypeError {
        match error {
            TypeError::TypeMismatch { types } => {
                reference_error(AdmissionReferenceError::Computed(
                    crate::validation::DeferredAccessTypeError::reference_mismatch(
                        crate::binding_index::RefTypeMismatch::direct(types),
                        self.reference.as_ref(),
                    ),
                ))
            }
            error => error,
        }
    }

    /// This entry is available only on reference evidence. Placement still
    /// requires `admit`, which also checks the boundary proof and actual value.
    #[cfg(test)]
    pub(super) fn judge_assignment<K: AssignmentKind, P: PlaceholderKind>(
        &self,
        destination: &Destination<K>,
        context: &AdmissionContext<'_, P, B>,
    ) -> Result<AssignmentVerdict<K>, AdmissionErrors> {
        let Some(contract) = destination.contract.as_deref() else {
            return Ok(AssignmentVerdict::NeedsSchema(
                SchemaObligation::destination(),
            ));
        };
        let mut findings = crate::schema::value_validation::AssignmentFindings::default();
        self.judge_at::<K>(
            contract,
            contract.attr(),
            &contract.path,
            &ReferenceContext::from(context),
            &mut findings,
        )
        .map_err(|error| {
            constraints(vec![AdmissionFinding {
                path: contract.path.clone(),
                error,
            }])
        })?;
        Ok(verdict(destination, findings))
    }

    pub(super) fn judge_at<K: AssignmentKind>(
        &self,
        contract: &DestinationContract,
        attr: &CompiledAttributeType,
        path: &LeafPath,
        context: &ReferenceContext<'_, B>,
        findings: &mut crate::schema::value_validation::AssignmentFindings,
    ) -> Result<(), TypeError> {
        #[cfg(test)]
        super::tests::reference_judged();
        let Some(bindings) = context.bindings else {
            findings
                .schema
                .requirements
                .push(SchemaRequirement::Reference);
            return Ok(());
        };
        let sink = contract.sink_type(attr, context.provider.catalog());
        if path == &contract.path
            && let Some((name, declared)) = context.declaration
        {
            let mismatch = |types| {
                reference_error(AdmissionReferenceError::Computed(
                    crate::validation::DeferredAccessTypeError::TypeMismatch {
                        types,
                        origin: format!("from {name}"),
                    },
                ))
            };
            let declaration = declared.declaration_type();
            let relation = K::reference_assignment(declaration, sink, self);
            if matches!(relation, AssignmentRelation::Mismatch) {
                return Err(mismatch(crate::schema::MismatchTypeNames::for_types(
                    *sink,
                    declaration,
                )));
            }
            let original = match declared.original().reference_assignment(sink, |source| {
                K::reference_original_assignment(source, sink, self)
            }) {
                Ok(relation) => relation,
                Err(_) if declared.incoming_mismatch(bindings) => return Ok(()),
                Err(error) => return Err(mismatch(error.type_names().clone())),
            };
            match AssignmentRelation::all([relation, original]) {
                AssignmentRelation::Assignable => {}
                AssignmentRelation::NeedsLocal(missing) => {
                    defer_reference_check::<K>(path, findings);
                    if let Some(missing) = missing {
                        findings
                            .schema
                            .requirements
                            .extend(missing.into_types().map(SchemaRequirement::Type));
                    }
                }
                AssignmentRelation::Unchecked(missing) => findings
                    .schema
                    .requirements
                    .extend(missing.into_types().map(SchemaRequirement::Type)),
                AssignmentRelation::Mismatch => {
                    return Err(mismatch(crate::schema::MismatchTypeNames::for_types(
                        *sink,
                        declaration,
                    )));
                }
            }
            return Ok(());
        }
        let source = match self.reference.as_ref() {
            crate::resource::ReferencePathRef::Access(path) => bindings.postfix_ref_type(path),
            crate::resource::ReferencePathRef::Binding(binding) => bindings.ref_type(binding),
            reference @ crate::resource::ReferencePathRef::BindingAccess { .. } => {
                bindings.ref_type(reference)
            }
        };
        match source {
            RefType::Typed(source) => {
                // The value walk supplies each Union member's own sink. Every
                // kind judges that sink even when several members share a shape:
                // reject if none fit, admit if all fit, without guessing a sink.
                if source.resource_handle_type().is_some() {
                    return Err(reference_error(AdmissionReferenceError::ResourceHandle {
                        reference: self.reference.as_ref().to_dot_string(),
                        types: source.mismatch_type_names(*sink),
                    }));
                }
                match AssignmentRelation::all(
                    source
                        .constraints()
                        .map(|source| K::reference_assignment(source, sink, self)),
                ) {
                    AssignmentRelation::Assignable => {}
                    AssignmentRelation::NeedsLocal(missing) => {
                        defer_reference_check::<K>(path, findings);
                        if let Some(missing) = missing {
                            findings
                                .schema
                                .requirements
                                .extend(missing.into_types().map(SchemaRequirement::Type));
                        }
                    }
                    AssignmentRelation::Unchecked(missing) => findings
                        .schema
                        .requirements
                        .extend(missing.into_types().map(SchemaRequirement::Type)),
                    AssignmentRelation::Mismatch => {
                        return Err(TypeError::TypeMismatch {
                            types: source.mismatch_type_names(*sink),
                        });
                    }
                }
            }
            RefType::Unchecked => {
                // A known binding without representable type authority cannot
                // confer placement. The owned obligation is retried on discharge.
                findings
                    .schema
                    .requirements
                    .push(SchemaRequirement::Reference);
            }
            RefType::UnknownBinding { binding, attribute } => {
                return Err(reference_error(AdmissionReferenceError::UnknownBinding {
                    binding,
                    attribute,
                }));
            }
            RefType::UnknownAttribute(crate::binding_index::RefTypeError::InvalidAccess {
                step,
                actual,
                ..
            }) => {
                return Err(reference_error(AdmissionReferenceError::Computed(
                    crate::validation::DeferredAccessTypeError::InvalidAccess {
                        error: crate::validation::inference::InferenceError::InvalidAccess {
                            base_type: actual,
                            step,
                        },
                    },
                )));
            }
            RefType::UnknownAttribute(error) => {
                return Err(reference_error(AdmissionReferenceError::Path(error)));
            }
        }
        Ok(())
    }
}

pub(super) fn reference_error(error: AdmissionReferenceError) -> TypeError {
    TypeError::Reference {
        error: Box::new(crate::resource::SecretDiagnosticError::Public(error)),
        attribute: None,
    }
}

fn defer_reference_check<K: AssignmentKind>(
    path: &LeafPath,
    findings: &mut crate::schema::value_validation::AssignmentFindings,
) {
    match K::defer_reference_check(path) {
        Some(local) => findings.local.push(local),
        None => findings.schema.require_reference(),
    }
}

fn verdict<K: AssignmentKind>(
    destination: &Destination<K>,
    findings: crate::schema::value_validation::AssignmentFindings,
) -> AssignmentVerdict<K> {
    let crate::schema::value_validation::AssignmentFindings {
        local: paths,
        schema,
        errors: _,
        deferred: _,
    } = findings;
    match K::local(destination, paths, schema) {
        Ok(obligation) => AssignmentVerdict::NeedsLocal(obligation),
        Err(schema) if schema.is_empty() => AssignmentVerdict::Proven(AssignmentProof {
            destination: destination.clone(),
        }),
        Err(schema) => AssignmentVerdict::NeedsSchema(schema),
    }
}

pub(super) struct AssignmentJudgement<K: AssignmentKind> {
    pub(super) verdict: AssignmentVerdict<K>,
    pub(super) findings: crate::schema::ValueValidationFindings,
}

pub(super) fn judge_with_findings<K: AssignmentKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    candidate: &Candidate<P, B>,
    context: &AdmissionContext<'_, P, B>,
) -> Result<AssignmentJudgement<K>, AdmissionErrors> {
    if candidate.obligations.identity != context.identity {
        return Err(KnowledgeError::<B>::InvalidStage {
            operation: "candidate/proof pairing",
        }
        .into());
    }
    if !destination
        .identity
        .revision
        .0
        .same_snapshot(&context.provider.catalog().custom_types)
    {
        return Err(KnowledgeError::<B>::InvalidStage {
            operation: "destination catalog revision",
        }
        .into());
    }
    if let AdmissionProof::Validation(proof) = &context.proof
        && let Some(error) = proof.failure()
    {
        return Err(error.into());
    }
    let Some(contract) = destination.contract.as_deref() else {
        if let Some(check) = context.untyped {
            let errors = check();
            if !errors.is_empty() {
                let checked = crate::schema::value_validation::untyped_symbolic_errors(errors);
                let mut error = constraints(
                    checked
                        .clone()
                        .into_located()
                        .into_iter()
                        .map(|(path, error)| AdmissionFinding { path, error })
                        .collect(),
                );
                error.findings = Some(Box::new(checked));
                return Err(error);
            }
        }
        return Ok(AssignmentJudgement {
            verdict: AssignmentVerdict::NeedsSchema(SchemaObligation::destination()),
            findings: Default::default(),
        });
    };
    let lookup = contract.lookup(context.provider.catalog());
    let mut checked = match &context.proof {
        AdmissionProof::Validation(proof) => {
            let reference_context = ReferenceContext::from(context);
            let authored =
                |attr: &CompiledAttributeType,
                 checked: crate::schema::value_validation::CheckValue<'_, P, B>,
                 path: &LeafPath,
                 cursor: crate::parser::retained::AuthoredCursor<'_, B>| {
                    match context.assignment.authored {
                        Some(hook) => hook(attr, checked, &join(&contract.path, path), cursor),
                        None => Ok(Default::default()),
                    }
                };
            let check =
                |attr: &CompiledAttributeType,
                 checked: crate::schema::value_validation::CheckValue<'_, P, B>,
                 path: &LeafPath,
                 cursor: crate::parser::retained::AuthoredCursor<'_, B>| {
                    let path = join(&contract.path, path);
                    let mut findings = Default::default();
                    // Selection parents are policy-only: main's ordinary local
                    // and reference obligations belong to the selected child.
                    if cursor.is_selection() {
                        return Ok(findings);
                    }
                    let crate::schema::value_validation::CheckValue::Current(value) = checked
                    else {
                        return Ok(findings);
                    };
                    match CandidateOrigin::of(value) {
                        CandidateOrigin::Reference(reference) => reference.judge_at::<K>(
                            contract,
                            attr,
                            &path,
                            &reference_context,
                            &mut findings,
                        )?,
                        CandidateOrigin::Expression => {
                            if let Some(check) = context.assignment.ordinary {
                                findings.extend(check(attr, value, &path, cursor)?);
                            }
                        }
                    }
                    Ok(findings)
                };
            let reference_check =
                |attr: &CompiledAttributeType,
                 relative_path: &LeafPath,
                 source: crate::parser::retained::SourceReference<'_, B>| {
                    let reference = ReferenceCandidate {
                        reference: source.into_path(),
                    };
                    // judge_at uses context.declaration only when supplied; resource attributes pass None.
                    reference
                        .judge_at::<K>(
                            contract,
                            attr,
                            &join(&contract.path, relative_path),
                            &reference_context,
                            // The ordinary path already checked this concrete
                            // value. Recovered source authority adds rejections,
                            // never new local or schema deferrals.
                            &mut Default::default(),
                        )
                        .map_err(|error| reference.error_with_origin(error))?;
                    Ok(())
                };
            crate::schema::value_validation::validate_authored_value_for_admission_with_references(
                contract.attr(),
                proof,
                contract.defs(),
                contract.ancestry,
                &lookup,
                context.cursor.clone(),
                crate::schema::value_validation::AssignmentCallbacks {
                    ordinary: &check,
                    authored: context.assignment.authored.map(|_| &authored as _),
                },
                crate::schema::value_validation::ReferenceCheck {
                    schema_source: &|binding: &B| {
                        reference_context.bindings.is_some_and(|bindings| {
                            bindings.is_schema_attribute_source(binding.as_str())
                        })
                    },
                    check: &reference_check,
                },
                &mut context.coverage.borrow_mut(),
            )
        }
        AdmissionProof::Plan(proof) => {
            crate::schema::value_validation::validate_authored_value_for_admission(
                contract.attr(),
                proof.knowledge(),
                contract.defs(),
                contract.ancestry,
                &lookup,
                context.cursor.clone(),
                crate::schema::value_validation::AssignmentCallbacks {
                    ordinary: &|_, _, _, _| Ok(Default::default()),
                    authored: None,
                },
                &mut context.coverage.borrow_mut(),
            )
        }
        AdmissionProof::Apply(proof) => {
            crate::schema::value_validation::validate_authored_value_for_admission(
                contract.attr(),
                &crate::resource::knowledge::Classification::<P, B>::Known(proof.clone()),
                contract.defs(),
                contract.ancestry,
                &lookup,
                context.cursor.clone(),
                crate::schema::value_validation::AssignmentCallbacks {
                    ordinary: &|_, _, _, _| Ok(Default::default()),
                    authored: None,
                },
                &mut context.coverage.borrow_mut(),
            )
        }
    };
    let mut assignment = checked.take_assignment();
    // Apply just rechecked the known value against the complete destination
    // contract above. That discharges missing reference/destination evidence;
    // any earlier type obligation still needs its provider authority below.
    if !context.is_apply() {
        assignment
            .schema
            .extend(candidate.obligations.schema.clone());
    }
    let mut errors = Vec::new();
    for (path, error) in checked.clone().into_located() {
        errors.push(AdmissionFinding {
            path: join(&contract.path, &path),
            error: error.with_attribute(&destination.label),
        });
    }
    if errors.is_empty()
        && context.is_apply()
        && candidate
            .obligations
            .schema
            .has_unavailable_type(context.provider.catalog())
    {
        return Err(constraints(vec![AdmissionFinding {
            path: contract.path.clone(),
            error: TypeError::from(crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
                .with_attribute(&destination.label),
        }]));
    }
    if !errors.is_empty() {
        let mut error = constraints(errors);
        error.findings = Some(Box::new(checked));
        return Err(error);
    }
    Ok(AssignmentJudgement {
        verdict: verdict(destination, assignment),
        findings: checked,
    })
}

#[cfg(test)]
mod discharge_tests {
    use super::*;
    use crate::binding_index::BindingValueSource;
    use crate::schema::{AttrTypeKind, AttributeType, TypeIdentity};
    use crate::validation::deferred_types::with_session_for_test;
    use std::collections::HashMap;

    fn text(value: &str) -> Value {
        Value::Concrete(crate::resource::ConcreteValue::String(value.into()))
    }

    fn pending_schema(pending: &PendingAdmission<kind::ResourceAttribute>) -> &SchemaObligation {
        let AssignmentVerdict::NeedsSchema(schema) = &pending.inner.assignment else {
            panic!("expected a pending schema obligation");
        };
        schema
    }

    fn check_discharge(
        pending: PendingAdmission<kind::ResourceAttribute>,
        session: &dyn ProviderTypeCheckContext,
    ) {
        assert!(!pending_schema(&pending).is_empty());
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("too long"))]),
            BindingValueSource::Local,
        );
        let error = pending.clone().discharge(&resolved, session).unwrap_err();
        let ResolutionError::AdmissionConstraints(findings) = error.resolution_error() else {
            panic!("expected a destination error: {error:?}");
        };
        assert_eq!(findings.findings()[0].path.to_string(), "target");
        assert!(matches!(
            findings.findings()[0].error,
            TypeError::LengthOutOfRange { max: Some(4), .. }
        ));
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("fits"))]),
            BindingValueSource::Local,
        );
        assert_eq!(
            pending.discharge(&resolved, session).unwrap().value(),
            &text("fits")
        );
    }

    fn target() -> AttributeType {
        AttributeType::refined_string(None, None, Some((None, Some(4))), None)
    }

    #[test]
    fn discharge_rejudges_reference_without_validation_bindings() {
        super::super::tests::fixture(
            AttributeType::string(),
            target(),
            |catalog, _, _, destination| {
                with_session_for_test(catalog, |session| {
                    let value = Value::root_resource_ref("producer", "output", vec![]);
                    let (candidate, context) = Candidate::validation_classified(
                        value
                            .knowledge(&KnowledgeContext::validation())
                            .unwrap()
                            .into_classification(),
                        None,
                        session,
                    );
                    let pending = admit(destination, candidate, &context)
                        .unwrap()
                        .ready()
                        .unwrap_err();
                    assert_eq!(
                        pending_schema(&pending).requirements,
                        [SchemaRequirement::Reference]
                    );
                    check_discharge(pending, session);
                });
            },
        );
    }

    #[test]
    fn discharge_rejudges_unchecked_reference_type() {
        super::super::tests::fixture(
            AttributeType::string(),
            target(),
            |catalog, schemas, _, destination| {
                let parsed = crate::parser::parse_file(
                    "let producer = upstream_state { source = './up' }",
                    std::path::Path::new("unchecked.crn"),
                    catalog,
                )
                .unwrap()
                .resolve_root_for_test();
                let bindings = BindingIndex::from_parsed(&parsed, schemas, catalog);
                assert!(matches!(
                    bindings.ref_type(&crate::resource::AccessPath::root("producer", "output")),
                    RefType::Unchecked
                ));
                with_session_for_test(catalog, |session| {
                    let value = Value::root_resource_ref("producer", "output", vec![]);
                    let (candidate, context) = Candidate::validation_classified(
                        value
                            .knowledge(&KnowledgeContext::validation())
                            .unwrap()
                            .into_classification(),
                        Some(&bindings),
                        session,
                    );
                    let pending = admit(destination, candidate, &context)
                        .unwrap()
                        .ready()
                        .unwrap_err();
                    assert_eq!(
                        pending_schema(&pending).requirements,
                        [SchemaRequirement::Reference]
                    );
                    check_discharge(pending, session);
                });
            },
        );
    }

    #[test]
    fn discharge_rejudges_unchecked_assignability() {
        let source = CompiledAttributeType {
            kind: AttrTypeKind::UnloadedSchemaType(
                crate::parser::UnloadedSchemaType::try_from(TypeIdentity::bare("SourceId"))
                    .unwrap(),
            ),
        };
        super::super::tests::fixture(
            AttributeType::string(),
            target(),
            |catalog, _, _, destination| {
                with_session_for_test(catalog, |session| {
                    let value = Value::root_resource_ref("producer", "output", vec![]);
                    let proof = PlanResolved::try_from(
                        value
                            .knowledge(&KnowledgeContext::plan(&ResolvedBindings::default()))
                            .unwrap(),
                    )
                    .unwrap();
                    let (mut candidate, context) = Candidate::plan(proof, session);
                    let contract = destination.contract.as_deref().unwrap();
                    let crate::schema::TypeAssignability::Unchecked(missing) = source
                        .in_schema(contract.defs())
                        .assignment_to(contract.sink_type(contract.attr(), catalog))
                    else {
                        panic!("unloaded source annotation must defer assignment");
                    };
                    candidate
                        .obligations
                        .schema
                        .requirements
                        .extend(missing.into_types().map(SchemaRequirement::Type));
                    let pending = admit(destination, candidate, &context)
                        .unwrap()
                        .ready()
                        .unwrap_err();
                    assert!(matches!(
                        pending_schema(&pending).requirements.as_slice(),
                        [SchemaRequirement::Type(_)]
                    ));
                    check_discharge(pending, session);
                });
            },
        );
    }

    #[test]
    fn discharge_rejudges_inherited_destination_schema_obligation() {
        super::super::tests::fixture(
            AttributeType::string(),
            target(),
            |catalog, _, _, destination| {
                with_session_for_test(catalog, |session| {
                    let value = Value::root_resource_ref("producer", "output", vec![]);
                    let proof = PlanResolved::try_from(
                        value
                            .knowledge(&KnowledgeContext::plan(&ResolvedBindings::default()))
                            .unwrap(),
                    )
                    .unwrap();
                    let (mut candidate, context) = Candidate::plan(proof, session);
                    // Retain an earlier destination obligation (for example a
                    // ProvidersNotLoaded finding) while its current contract fits.
                    candidate.obligations.schema = SchemaObligation::destination();
                    let pending = admit(destination, candidate, &context)
                        .unwrap()
                        .ready()
                        .unwrap_err();
                    check_discharge(pending, session);
                });
            },
        );
    }
}
