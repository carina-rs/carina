//! The existing assignment relation and value validator make one decision.

use super::*;
use crate::binding_index::RefType;
use crate::resource::{DeferredValue, ReferencePath};
use crate::schema::SinkAuthority;
use crate::schema::assignability::AssignmentRelation;

#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentProof<K: DestinationKind> {
    destination: Destination<K>,
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssignmentVerdict<K: DestinationKind> {
    Proven(AssignmentProof<K>),
    NeedsLocal(K::LocalObligation),
    NeedsSchema(SchemaObligation),
}

/// Only boundary factories can select this origin. In particular, a future
/// conversion's judged type cannot manufacture a reference-origin token.
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
    reference: ReferencePath<B>,
}

impl<B: BindingAddress> ReferenceCandidate<B> {
    /// This entry is available only on reference evidence. Placement still
    /// requires `admit`, which also checks the boundary proof and actual value.
    pub fn judge_assignment<K: DestinationKind, P: PlaceholderKind>(
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
        self.judge_at(
            destination,
            &contract.attr,
            &contract.path,
            context,
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

    fn judge_at<K: DestinationKind, P: PlaceholderKind>(
        &self,
        destination: &Destination<K>,
        attr: &CompiledAttributeType,
        path: &LeafPath,
        context: &AdmissionContext<'_, P, B>,
        findings: &mut crate::schema::value_validation::AssignmentFindings,
    ) -> Result<(), TypeError> {
        let Some(bindings) = context.bindings else {
            return Ok(());
        };
        let contract = destination.contract.as_deref().expect("attribute contract");
        let sink = attr.in_schema(&contract.defs).with_authority(
            &context.provider.catalog().custom_types,
            SinkAuthority::Owner(&contract.owner),
        );
        match bindings.ref_type(self.reference.as_ref()) {
            RefType::Typed(source) => {
                match AssignmentRelation::all(
                    source
                        .constraints()
                        .map(|source| K::reference_assignment(source, sink, self)),
                ) {
                    AssignmentRelation::Assignable => {}
                    AssignmentRelation::NeedsLocal(missing) => {
                        findings.local.push(path.clone());
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
            RefType::UnknownAttribute(error) => {
                return Err(reference_error(AdmissionReferenceError::Path(error)));
            }
        }
        Ok(())
    }
}

fn reference_error(error: AdmissionReferenceError) -> TypeError {
    TypeError::Reference {
        error: Box::new(crate::resource::SecretDiagnosticError::Public(error)),
        attribute: None,
    }
}

fn verdict<K: DestinationKind>(
    destination: &Destination<K>,
    findings: crate::schema::value_validation::AssignmentFindings,
) -> AssignmentVerdict<K> {
    let crate::schema::value_validation::AssignmentFindings {
        local: paths,
        schema,
    } = findings;
    if !paths.is_empty() {
        // Only the sealed migrated kinds can have produced these paths.
        let obligation =
            K::local(destination, paths, schema).expect("reference policy owns local obligations");
        AssignmentVerdict::NeedsLocal(obligation)
    } else if schema.is_empty() {
        AssignmentVerdict::Proven(AssignmentProof {
            destination: destination.clone(),
        })
    } else {
        AssignmentVerdict::NeedsSchema(schema)
    }
}

pub fn judge_assignment<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
    destination: &Destination<K>,
    candidate: &Candidate<P, B>,
    context: &AdmissionContext<'_, P, B>,
) -> Result<AssignmentVerdict<K>, AdmissionErrors> {
    if candidate.identity != context.identity {
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
        return Ok(AssignmentVerdict::NeedsSchema(
            SchemaObligation::destination(),
        ));
    };
    let check = |attr: &CompiledAttributeType, value: &Value<P, B>, path: &LeafPath| {
        let path = join(&contract.path, path);
        let origin = if std::ptr::eq(
            value,
            crate::resource::knowledge::ClassifiedRef::from(
                &**context.validation_proof().expect("validation proof"),
            )
            .diagnostic_value(),
        ) {
            candidate.origin.clone()
        } else {
            CandidateOrigin::of(value)
        };
        let mut findings = crate::schema::value_validation::AssignmentFindings::default();
        match origin {
            CandidateOrigin::Reference(reference) => {
                reference.judge_at(destination, attr, &path, context, &mut findings)?
            }
            CandidateOrigin::Expression => {}
        }
        Ok(findings)
    };
    let lookup = ContractLookup {
        owner: &contract.owner,
        resolver: context.provider.catalog().custom_type_resolver(),
    };
    let mut checked = match &context.proof {
        AdmissionProof::Validation(proof) => {
            crate::schema::value_validation::validate_value_for_admission(
                &contract.attr,
                proof,
                &contract.defs,
                contract.ancestry,
                &lookup,
                Some(&check),
            )
        }
        AdmissionProof::Plan(proof) => crate::schema::value_validation::validate_value_with_lookup(
            &contract.attr,
            proof.knowledge(),
            &contract.defs,
            contract.ancestry,
            &lookup,
        ),
        AdmissionProof::Apply(proof) => {
            crate::schema::value_validation::validate_value_with_lookup(
                &contract.attr,
                &crate::resource::knowledge::Classification::<P, B>::Known(proof.clone()),
                &contract.defs,
                contract.ancestry,
                &lookup,
            )
        }
    };
    let mut assignment = checked.take_assignment();
    assignment.schema.extend(candidate.schema.clone());
    let mut errors = Vec::new();
    for (path, error) in checked.into_located() {
        match error {
            TypeError::CustomTypeValidation {
                ref error,
                attribute: _,
            } if matches!(
                error.as_ref(),
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded
            ) =>
            {
                assignment
                    .schema
                    .requirements
                    .push(SchemaRequirement::Destination);
            }
            error => errors.push(AdmissionFinding {
                path: join(&contract.path, &path),
                error: error.with_attribute(&destination.label),
            }),
        }
    }
    if !errors.is_empty() {
        return Err(constraints(errors));
    }
    Ok(verdict(destination, assignment))
}

/// Owner authority comes from the descriptor; a caller cannot replace it.
struct ContractLookup<'a> {
    owner: &'a crate::parser::SchemaTypeOwner,
    resolver: &'a dyn crate::parser::CustomTypeResolver,
}
impl crate::schema::ValidatedCustomTypeLookup for ContractLookup<'_> {
    fn canonicalization_error(
        &self,
        declaration: crate::parser::IdentifiedCustomTypeDeclaration<'_>,
        error: TypeError,
    ) -> TypeError {
        match self.owner {
            crate::parser::SchemaTypeOwner::Provider(owner) => {
                crate::parser::custom_type_canonicalization_error(owner, declaration, error)
            }
        }
    }
    fn validate(
        &self,
        value: crate::schema::LocallyValidatedCustomType<'_>,
    ) -> Result<(), TypeError> {
        match self.owner {
            crate::parser::SchemaTypeOwner::Provider(owner) => self
                .resolver
                .validate_validated(owner, value)
                .map_err(Into::into),
        }
    }
}
