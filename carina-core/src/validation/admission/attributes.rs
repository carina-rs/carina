//! Catalog-bound resource and data-source attribute descriptors.

use super::*;
use crate::resource::ParserPlaceholder;
use crate::resource::ResourceId;
use crate::schema::LeafPathSegment;
#[cfg(test)]
use crate::schema::SchemaRegistry;

mod inputs;
use inputs::{AttributeInput, with_classified_inputs};
pub use inputs::{
    ClassificationDiagnostics, ResourceAdmissionOutcome, ResourceAttributeInputs, SlotPrechecks,
    ValidationEvaluation, classification_preflight,
};

pub(crate) fn declaration_identity(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
) -> DeclarationIdentity {
    DeclarationIdentity {
        owner: DeclarationOwner::Provider(schema.schema_type_owner().clone()),
        schema: schema.schema().admission_identity,
    }
}

fn identity(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    catalog: &ProviderContext,
) -> DestinationIdentity {
    DestinationIdentity {
        declaration: DeclarationIdentity {
            owner: DeclarationOwner::Provider(schema.schema_type_owner().clone()),
            schema: schema.schema().admission_identity,
        },
        instance: InstanceIdentity::Resource(id.clone()),
        path: vec![],
        revision: CatalogRevision(CatalogSnapshot::Provider(catalog.custom_types.clone())),
    }
}

impl crate::schema::OwnerBoundResourceSchema<'_> {
    fn untyped_attribute<K: AssignmentKind>(
        self,
        id: &ResourceId,
        name: &str,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        Destination::catalogued(identity(self, id, catalog), name.into())
    }

    #[cfg(test)]
    pub(super) fn admission_selected_attribute_for_test<K: AssignmentKind>(
        self,
        id: &ResourceId,
        attribute: &str,
        selected: crate::schema::value_validation::SchemaAttribute<'_>,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        self.admission_selected_attribute(id, attribute, selected, catalog)
    }

    fn admission_selected_attribute<K: AssignmentKind>(
        self,
        id: &ResourceId,
        attribute: &str,
        selected: crate::schema::value_validation::SchemaAttribute<'_>,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        let crate::schema::value_validation::SchemaAttribute { ordinal, effective } = selected;
        let mut destination =
            Destination::catalogued(identity(self, id, catalog), attribute.into());
        destination.identity.path.push(ordinal);
        #[cfg(test)]
        super::tests::contract_constructed();
        destination.contract = Some(Box::new(DestinationContract::resource(
            self.schema().shared_schema(),
            effective.name().into(),
            self.schema_type_owner().clone(),
            FieldAncestry::from_input_mode(effective.effective_input_mode()),
            LeafPath::from_segments([LeafPathSegment::StructField(attribute.into())]),
        )));
        destination
    }
}

fn admission_findings<K: AssignmentKind, P: PlaceholderKind, B: BindingAddress>(
    result: &mut Result<Admission<K, P, B>, AdmissionErrors>,
) -> crate::schema::ValueValidationFindings {
    match result {
        Ok(admission) => admission.take_findings(),
        Err(error) => error.take_findings().unwrap_or_else(|| {
            let error = match error.resolution_error().clone() {
                ResolutionError::Knowledge(error) => TypeError::Knowledge {
                    error,
                    attribute: None,
                },
                resolution => TypeError::Reference {
                    error: Box::new(crate::resource::SecretDiagnosticError::Public(
                        AdmissionReferenceError::Resolution(Box::new(resolution)),
                    )),
                    attribute: None,
                },
            };
            crate::schema::value_validation::classification_error(error)
        }),
    }
}

/// Plan uses only the plan-stage proof and retains diagnostic findings. Its
/// pending admissions are dropped; apply re-admits the resolved resource values.
pub fn admit_plan_attributes<K: RechecksResolved>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    resource: &ResourceId,
    attributes: &indexmap::IndexMap<String, PlanResolved<'_>>,
    quoted: &std::collections::HashSet<String>,
    session: &dyn ProviderTypeCheckContext,
) -> crate::schema::ResourceValueValidation {
    let projected = attributes
        .iter()
        .map(|(name, proof)| (name.clone(), proof.knowledge().clone()))
        .collect();
    schema
        .schema()
        .as_schema()
        .validate_resolved_admission_report(
            &projected,
            None,
            &|name| quoted.contains(name),
            &|name, selected, _| {
                let destination = schema.admission_selected_attribute::<K>(
                    resource,
                    name,
                    selected,
                    session.catalog(),
                );
                let (candidate, boundary) = Candidate::plan(attributes[name].clone(), session);
                admission_findings(&mut admit(&destination, candidate, &boundary))
            },
        )
}

#[derive(Debug)]
pub(crate) enum ApplyAdmissionErrors {
    Admission(AdmissionErrors),
    Constraints(Box<crate::schema::ResourceValueValidation>),
}
impl From<AdmissionErrors> for ApplyAdmissionErrors {
    fn from(error: AdmissionErrors) -> Self {
        Self::Admission(error)
    }
}
impl From<KnowledgeError<AbsoluteBindingAddress>> for ApplyAdmissionErrors {
    fn from(error: KnowledgeError<AbsoluteBindingAddress>) -> Self {
        Self::Admission(error.into())
    }
}

/// The resource itself owns the candidate input. Both live and saved apply enter here.
pub(crate) fn admit_apply_resource<S, D>(
    resource: &crate::resource::Resource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    authored: Option<(
        &std::collections::HashMap<String, Value>,
        &std::collections::HashSet<String>,
    )>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    bindings: &ResolvedBindings,
    session: &dyn ProviderTypeCheckContext,
) -> Result<(), ApplyAdmissionErrors> {
    admit_apply_attributes::<kind::ResourceAttribute>(
        &resource.id,
        &resource.attributes,
        authored,
        &resource.quoted_string_attrs,
        schema,
        bindings,
        session,
    )
}

pub(crate) fn admit_apply_data_source<S, D>(
    resource: &crate::resource::DataSource<S, NoPlaceholder, AbsoluteBindingAddress, D>,
    authored: Option<(
        &std::collections::HashMap<String, Value>,
        &std::collections::HashSet<String>,
    )>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    bindings: &ResolvedBindings,
    session: &dyn ProviderTypeCheckContext,
) -> Result<(), ApplyAdmissionErrors> {
    admit_apply_attributes::<kind::DataSourceAttribute>(
        &resource.id,
        &resource.attributes,
        authored,
        &resource.quoted_string_attrs,
        schema,
        bindings,
        session,
    )
}

#[allow(clippy::too_many_arguments)]
fn admit_apply_attributes<K: RechecksResolved>(
    id: &ResourceId,
    resource_attributes: &indexmap::IndexMap<String, Value>,
    authored: Option<(
        &std::collections::HashMap<String, Value>,
        &std::collections::HashSet<String>,
    )>,
    quoted: &std::collections::HashSet<String>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    bindings: &ResolvedBindings,
    session: &dyn ProviderTypeCheckContext,
) -> Result<(), ApplyAdmissionErrors> {
    use std::cell::RefCell;
    let mut projected = indexmap::IndexMap::new();
    let mut projection_errors = Vec::new();
    for (name, value) in resource_attributes {
        let value = match authored {
            Some((attributes, names)) => {
                let Some(authored_value) = attributes.get(name) else {
                    // Provider normalization can add keys at plan time. They
                    // are provider input, outside the authored validation set.
                    continue;
                };
                if names.contains(name) {
                    // Admission candidates come from the resolved resource.
                    value
                } else {
                    // Cross-attribute checks retain the plan-validated spelling
                    // of literals alongside the newly resolved candidates.
                    authored_value
                }
            }
            None => value,
        };
        match value.knowledge(&KnowledgeContext::apply(bindings)) {
            Ok(proof) => {
                projected.insert(name.clone(), proof);
            }
            Err(error) => projection_errors.push((name, AdmissionErrors::from(error))),
        }
    }
    let classified = projected
        .iter()
        .map(|(name, proof)| (name.clone(), (**proof).clone()))
        .collect();
    let failures = RefCell::new(Vec::new());
    let mut report = schema
        .schema()
        .as_schema()
        .validate_resolved_admission_report(
            &classified,
            authored.map(|(_, names)| names),
            &|name| quoted.contains(name),
            &|name, selected, _| {
                let destination =
                    schema.admission_selected_attribute::<K>(id, name, selected, session.catalog());
                let result = Candidate::apply(projected[name].clone(), session).and_then(
                    |(candidate, boundary)| admit_apply(&destination, candidate, &boundary),
                );
                match result {
                    Ok(mut placed) => placed.take_findings(),
                    Err(mut error) => match error.take_findings() {
                        Some(findings) => findings,
                        None => {
                            let findings =
                                admission_findings::<K, NoPlaceholder, AbsoluteBindingAddress>(
                                    &mut Err(error.clone()),
                                );
                            failures.borrow_mut().push(error);
                            findings
                        }
                    },
                }
            },
        );
    let mut failures = failures.into_inner();
    for (name, error) in projection_errors {
        for finding in
            admission_findings::<K, NoPlaceholder, AbsoluteBindingAddress>(&mut Err(error.clone()))
                .local_errors()
        {
            report.push_attribute_error(name, finding.clone());
        }
        failures.push(error);
    }
    // Preserve the established single unresolved-input boundary error. Mixed
    // or multiple failures are returned with the complete attribute report.
    if failures.len() == 1 && report.errors().count() == 1 {
        return Err(ApplyAdmissionErrors::Admission(failures.pop().unwrap()));
    }
    if report.errors().next().is_some() {
        return Err(ApplyAdmissionErrors::Constraints(Box::new(report)));
    }
    Ok(())
}

/// The retained expression check used by the authored attribute walk.
type ValidationAssignmentCheck<'a, P, B> = Option<
    &'a dyn Fn(
        &str,
        &CompiledAttributeType,
        &Value<P, B>,
        &LeafPath,
        &DestinationContract,
        &assignment::ReferenceContext<'_, B>,
        crate::parser::retained::AuthoredCursor<'_, B>,
    ) -> Result<crate::schema::value_validation::AssignmentFindings, TypeError>,
>;

pub(crate) struct SchemaInputAuthority<'a, P: PlaceholderKind, B: BindingAddress> {
    source: SchemaInputSource<'a, P, B>,
    report: &'a crate::parser::retained::ExpressionReport<B>,
    coverage: &'a mut WalkCoverage,
}
enum SchemaInputSource<'a, P: PlaceholderKind, B: BindingAddress> {
    Programmatic(&'a crate::identifier::CanonicalAttributeValues<P, B>),
    Authored {
        slots: &'a crate::parser::retained::AuthoredSlots<B>,
        values: &'a crate::identifier::CanonicalAttributeValues<P, B>,
    },
}
impl<'a, P: PlaceholderKind, B: BindingAddress> SchemaInputAuthority<'a, P, B> {
    pub(crate) fn new(
        slots: &'a crate::parser::retained::AuthoredSlots<B>,
        values: &'a crate::identifier::CanonicalAttributeValues<P, B>,
        report: &'a crate::parser::retained::ExpressionReport<B>,
        coverage: &'a mut WalkCoverage,
    ) -> Self {
        // Only the producer's lack of authored storage grants value-only
        // authority. An attribute lookup failure cannot construct this state.
        let source = match slots.declaration_id() {
            Some(_) => SchemaInputSource::Authored { slots, values },
            None => SchemaInputSource::Programmatic(values),
        };
        Self {
            source,
            report,
            coverage,
        }
    }
}

/// One authored attribute report, shared by CLI validation and LSP rendering.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_validation_attributes<
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    quoted: &std::collections::HashSet<String>,
    bindings: Option<&BindingIndex<'_, B>>,
    session: &dyn ProviderTypeCheckContext,
    check: ValidationAssignmentCheck<'_, P, B>,
    declarations: Option<
        &std::collections::HashMap<String, (String, crate::binding_index::DeclaredValueSource)>,
    >,
    authority: SchemaInputAuthority<'_, P, B>,
) -> ResourceAdmissionOutcome<B> {
    let SchemaInputAuthority {
        source,
        report,
        coverage,
    } = authority;
    let (slots, values) = match source {
        SchemaInputSource::Programmatic(values) => (None, values),
        SchemaInputSource::Authored { slots, values } => (Some(slots), values),
    };
    let mut classification_errors = Vec::new();
    let attributes = crate::resource::knowledge::collect_classified_attributes(
        values.iter().map(|(name, value)| (name.clone(), value)),
        &crate::resource::knowledge::KnowledgeContext::validation(),
        |name, error| classification_errors.push((name.to_owned(), error)),
    );
    let classification_diagnostics = ClassificationDiagnostics(classification_errors);
    let Some(slots) = slots else {
        return ResourceAdmissionOutcome {
            values: admit_classified_value_only_attributes::<K, P, B>(
                schema,
                id,
                &attributes,
                quoted,
                session,
                &values.awaiting,
            ),
            classification_diagnostics,
        };
    };
    let missing = inputs::missing_schema_inputs(slots, &attributes, report);
    let candidate = |name: &str, proof| {
        if let Some(projection) = slots.projected_phase(name) {
            return Ok(Box::new(Candidate::validation_projected(
                proof, bindings, session, projection,
            )));
        }
        let slot = slots
            .get(name)
            .or_else(|error| {
                schema
                    .schema()
                    .as_schema()
                    .attributes()
                    .get(name)
                    .and_then(|field| field.parts().block_name)
                    .map_or(Err(error), |alias| slots.get(alias))
            })
            .map_err(seam_constraint)?;
        let projection = match slot.source_judgement_in(report) {
            Ok(source) => crate::parser::retained::ValidationProjection::judged(source.clone()),
            Err(missing) => crate::parser::retained::ValidationProjection::unavailable(
                slot.source_judgement().map_err(seam_constraint)?,
                missing,
            )
            .map_err(seam_constraint)?,
        };
        Ok(Box::new(Candidate::validation_projected(
            proof, bindings, session, projection,
        )))
    };
    let visits = std::cell::RefCell::new(std::mem::take(coverage));
    let findings = admit_validation_attributes_with_input::<K, P, B>(
        schema,
        id,
        &attributes,
        attributes.iter(),
        quoted,
        bindings,
        session,
        check,
        declarations,
        &values.awaiting,
        ValidationInput::Resource {
            candidate: &candidate,
            coverage: &visits,
        },
    );
    *coverage = visits.into_inner();
    ResourceAdmissionOutcome {
        values: findings.map_value(|findings| missing.finish(findings, coverage)),
        classification_diagnostics,
    }
}

/// Programmatic values enter a NoAuthored walk, with no callback or conversion
/// channel. Parsed inputs must use the source-bearing factory above.
pub(crate) fn admit_value_only_attributes<
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    values: &crate::identifier::CanonicalAttributeValues<P, B>,
    quoted: &std::collections::HashSet<String>,
    session: &dyn ProviderTypeCheckContext,
) -> crate::identifier::PrefixAdmitted<crate::schema::ResourceValueValidation> {
    let mut classification_errors = Vec::new();
    let attributes = crate::resource::knowledge::collect_classified_attributes(
        values.iter().map(|(name, value)| (name.clone(), value)),
        &crate::resource::knowledge::KnowledgeContext::validation(),
        |name, error| classification_errors.push((name.to_owned(), TypeError::from(error))),
    );
    let report = admit_classified_value_only_attributes::<K, P, B>(
        schema,
        id,
        &attributes,
        quoted,
        session,
        &values.awaiting,
    );
    report.map_value(|mut report| {
        for (name, error) in classification_errors {
            report.push_attribute_error(&name, error);
        }
        report
    })
}

// Both callers classified a witnessed map, never an arbitrary authored map.
fn admit_classified_value_only_attributes<
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    attributes: &indexmap::IndexMap<String, crate::resource::knowledge::Classification<'_, P, B>>,
    quoted: &std::collections::HashSet<String>,
    session: &dyn ProviderTypeCheckContext,
    awaiting: &crate::identifier::PrefixObligations,
) -> crate::identifier::PrefixAdmitted<crate::schema::ResourceValueValidation> {
    schema
        .schema()
        .as_schema()
        .validate_admission_report_with_obligations(
            attributes,
            attributes.iter(),
            &|name| quoted.contains(name),
            &crate::schema::value_validation::AttributeAdmission {
                known: &|name, selected, proof| {
                    let destination = schema.admission_selected_attribute::<K>(
                        id,
                        name,
                        selected,
                        session.catalog(),
                    );
                    match destination.contract.as_deref() {
                        Some(contract) => crate::schema::value_validation::validate_value_locally(
                            contract.attr(),
                            proof,
                            contract.defs(),
                            contract.ancestry,
                            Some(&contract.lookup(session.catalog())),
                        ),
                        None => Default::default(),
                    }
                },
                unknown: None,
            },
            awaiting,
        )
}

type ValidationCandidateFactory<'a, P, B> =
    &'a dyn Fn(
        &str,
        crate::resource::knowledge::Classification<'a, P, B>,
    ) -> Result<Box<(Candidate<P, B>, AdmissionContext<'a, P, B>)>, AdmissionErrors>;

/// The classified path is for explicit schema-only producers. A live authored
/// source must supply its cursor factory; association failure is never projected.
enum ValidationInput<'a, P: PlaceholderKind, B: BindingAddress> {
    // Producer-owned inputs preserve pairing even on the report-only path.
    // AttributeInput, not analysis kind, selects the cursor authority.
    Resource {
        candidate: ValidationCandidateFactory<'a, P, B>,
        coverage: &'a std::cell::RefCell<crate::schema::value_validation::WalkCoverage>,
    },
}

/// One authored attribute report, shared by CLI validation and LSP rendering.
#[allow(clippy::too_many_arguments)]
fn admit_validation_attributes_with_input<
    'a,
    'v: 'a,
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    attributes: &'a indexmap::IndexMap<
        String,
        crate::resource::knowledge::Classification<'v, P, B>,
    >,
    entries: impl IntoIterator<
        Item = (
            &'a String,
            &'a crate::resource::knowledge::Classification<'v, P, B>,
        ),
    >,
    quoted: &std::collections::HashSet<String>,
    bindings: Option<&BindingIndex<'_, B>>,
    session: &dyn ProviderTypeCheckContext,
    check: ValidationAssignmentCheck<'_, P, B>,
    declarations: Option<
        &std::collections::HashMap<String, (String, crate::binding_index::DeclaredValueSource)>,
    >,
    awaiting: &crate::identifier::PrefixObligations,
    input: ValidationInput<'v, P, B>,
) -> crate::identifier::PrefixAdmitted<crate::schema::ResourceValueValidation> {
    let admit_attribute = |name: &str,
                           proof: &crate::resource::knowledge::Classification<'v, P, B>,
                           destination: &Destination<K>| {
        let (candidate, mut boundary) = match &input {
            ValidationInput::Resource { candidate, .. } => match candidate(name, proof.clone()) {
                Ok(input) => *input,
                Err(error) => return admission_findings(&mut Err::<Admission<K, P, B>, _>(error)),
            },
        };
        boundary.declaration = declarations
            .and_then(|sources| sources.get(name))
            .map(|(name, source)| (name.as_str(), source));
        // Retained expressions use the same attribute contract and binding
        // authority. Their leaf references have no top-level declaration.
        let reference_context = assignment::ReferenceContext {
            bindings,
            provider: session,
            declaration: None,
        };
        let assignment = destination.contract.as_deref().map(|contract| {
            move |attr: &CompiledAttributeType,
                  value: &Value<P, B>,
                  path: &LeafPath,
                  cursor: crate::parser::retained::AuthoredCursor<'_, B>| {
                match check {
                    Some(check) => check(
                        name,
                        attr,
                        value,
                        path,
                        contract,
                        &reference_context,
                        cursor,
                    ),
                    None => Ok(Default::default()),
                }
            }
        });
        let untyped = || {
            let Some(bindings) = bindings else {
                return Vec::new();
            };
            let interpolation_sink =
                crate::validation::lift_type_expr(&crate::validation::INTERPOLATION_REF_SINK)
                    .expect("built-in interpolation sink");
            let context = crate::validation::ResourceReferenceContext {
                location: crate::validation::ReferenceFindingLocation::Resource {
                    location: id.to_string(),
                    attribute: name.into(),
                },
                bindings,
                declared_source: None,
                interpolation_sink: &interpolation_sink,
            };
            crate::validation::collect_ref_diagnostics_with_sink(
                crate::resource::knowledge::ClassifiedRef::from(proof).diagnostic_value(),
                None,
                &mut |reference, sink, usage, _, errors| {
                    errors.extend(crate::validation::check_resource_reference(
                        &context, reference, sink, usage,
                    ));
                },
            )
            .into_iter()
            .map(|finding| {
                assignment::reference_error(AdmissionReferenceError::Authored(Box::new(finding)))
            })
            .collect()
        };
        let boundary = boundary.with_checks(
            super::AssignmentChecks {
                #[cfg(test)]
                authored: None,
                ordinary: assignment.as_ref().map(|check| check as &_),
            },
            &untyped,
        );
        let findings = admission_findings(&mut admit(destination, candidate, &boundary));
        let ValidationInput::Resource { coverage, .. } = &input;
        coverage.borrow_mut().extend(boundary.coverage.take());
        if proof.failure().is_some() {
            // Keep attempted failure evidence and attribute presence. The
            // expression/collector pass owns its validation diagnostic.
            findings.retained_failure()
        } else {
            findings
        }
    };
    schema
        .schema()
        .as_schema()
        .validate_admission_report_with_obligations(
            attributes,
            entries,
            &|name| quoted.contains(name),
            &crate::schema::value_validation::AttributeAdmission {
                known: &|name, selected, proof| {
                    let destination = schema.admission_selected_attribute::<K>(
                        id,
                        name,
                        selected,
                        session.catalog(),
                    );
                    admit_attribute(name, proof, &destination)
                },
                unknown: Some(&|name, proof| {
                    let destination = schema.untyped_attribute::<K>(id, name, session.catalog());
                    admit_attribute(name, proof, &destination)
                }),
            },
            awaiting,
        )
}

/// Exhaustive resource/data-source policy shared by all analysis adapters.
pub fn uses_resource_attribute_admission(kind: crate::parser::AnalysisItemKind) -> bool {
    use crate::parser::AnalysisItemKind;
    match kind {
        AnalysisItemKind::DataSource => false,
        AnalysisItemKind::Resource
        | AnalysisItemKind::Composition
        | AnalysisItemKind::DeferredResource
        | AnalysisItemKind::Wait => true,
    }
}

/// All inputs to one resource use. The run owner supplies coverage and tracks admission.
pub struct ResourceAdmissionRequest<'a, 'file, E, D, B: BindingAddress>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resource:
        &'a crate::parser::AnalysisResourceRef<'file, ParserPlaceholder, ParserPlaceholder, D, B>,
    parsed: crate::identifier::PrefixFileRead<'a, E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schema: crate::schema::OwnerBoundResourceSchema<'a>,
    quoted: &'a std::collections::HashSet<String>,
    inputs: ResourceAttributeInputs<B>,
    evaluation: ValidationEvaluation<B>,
    bindings: &'a BindingIndex<'a, B>,
    session: &'a dyn ProviderTypeCheckContext,
}

impl<'a, 'file, E, D, B: BindingAddress> ResourceAdmissionRequest<'a, 'file, E, D, B>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    pub(crate) fn resource(
        &self,
    ) -> &'a crate::parser::AnalysisResourceRef<'file, ParserPlaceholder, ParserPlaceholder, D, B>
    {
        self.resource
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_test(
        resource: &'a crate::parser::AnalysisResourceRef<
            'file,
            ParserPlaceholder,
            ParserPlaceholder,
            D,
            B,
        >,
        parsed: &'a crate::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
        schema: crate::schema::OwnerBoundResourceSchema<'a>,
        quoted: &'a std::collections::HashSet<String>,
        inputs: ResourceAttributeInputs<B>,
        evaluation: ValidationEvaluation<B>,
        bindings: &'a BindingIndex<'a, B>,
        session: &'a dyn ProviderTypeCheckContext,
        schemas: &'a SchemaRegistry,
    ) -> Self {
        Self {
            resource,
            parsed: crate::identifier::PrefixFileRead::for_test(parsed, schemas),
            schema,
            quoted,
            inputs,
            evaluation,
            bindings,
            session,
        }
    }

    pub fn new(
        owner: &'a crate::identifier::CanonicalResource<'file, E, D, B>,
        quoted: &'a std::collections::HashSet<String>,
        inputs: ResourceAttributeInputs<B>,
        evaluation: ValidationEvaluation<B>,
        bindings: &'a BindingIndex<'a, B>,
        session: &'a dyn ProviderTypeCheckContext,
    ) -> Result<Self, crate::parser::retained::MissingEvidence> {
        let schema = owner.resource_schema();
        if inputs.declaration.as_ref() != Some(&declaration_identity(schema)) {
            return Err(crate::parser::retained::MissingEvidence::new(
                "canonical resource schema/value pairing",
            ));
        }
        Ok(Self {
            resource: owner,
            parsed: owner.parsed(),
            schema,
            quoted,
            inputs,
            evaluation,
            bindings,
            session,
        })
    }
}

type ResourceAdmissionCandidate<'a, B> = (
    Candidate<ParserPlaceholder, B>,
    AdmissionContext<'a, ParserPlaceholder, B>,
);

trait ResourceAdmissionPass {
    fn missing_source(source: &crate::parser::retained::JudgedExpr) -> crate::ConversionChannel;

    fn candidate<'a, B: BindingAddress>(
        slot: &'a crate::parser::retained::JudgedSlot<B>,
        proof: crate::resource::knowledge::Classification<'a, ParserPlaceholder, B>,
        bindings: &'a BindingIndex<'a, B>,
        session: &'a dyn ProviderTypeCheckContext,
    ) -> Result<ResourceAdmissionCandidate<'a, B>, AdmissionErrors>;
}
struct LiveResource;
struct TemplateResource;
impl ResourceAdmissionPass for LiveResource {
    fn missing_source(source: &crate::parser::retained::JudgedExpr) -> crate::ConversionChannel {
        crate::parser::retained::ConversionVerdict::unavailable_source(source)
    }

    fn candidate<'a, B: BindingAddress>(
        slot: &'a crate::parser::retained::JudgedSlot<B>,
        proof: crate::resource::knowledge::Classification<'a, ParserPlaceholder, B>,
        bindings: &'a BindingIndex<'a, B>,
        session: &'a dyn ProviderTypeCheckContext,
    ) -> Result<ResourceAdmissionCandidate<'a, B>, AdmissionErrors> {
        Candidate::validation_slot(slot, proof, Some(bindings), session)
    }
}
impl ResourceAdmissionPass for TemplateResource {
    fn missing_source(_source: &crate::parser::retained::JudgedExpr) -> crate::ConversionChannel {
        // This separate parse is suppress-only; live instances admit values.
        Default::default()
    }

    fn candidate<'a, B: BindingAddress>(
        slot: &'a crate::parser::retained::JudgedSlot<B>,
        proof: crate::resource::knowledge::Classification<'a, ParserPlaceholder, B>,
        bindings: &'a BindingIndex<'a, B>,
        session: &'a dyn ProviderTypeCheckContext,
    ) -> Result<ResourceAdmissionCandidate<'a, B>, AdmissionErrors> {
        Candidate::template_preflight_slot(slot, proof, Some(bindings), session)
    }
}

pub(in crate::validation) fn resource_template_preflight_admissions<E, D, B: BindingAddress>(
    request: ResourceAdmissionRequest<'_, '_, E, D, B>,
    coverage: &mut WalkCoverage,
) -> ResourceAdmissionOutcome<B>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resource_admissions_for_pass::<TemplateResource, _, _, _>(request, coverage)
}
fn resource_admissions_for_pass<Pass: ResourceAdmissionPass, E, D, B: BindingAddress>(
    request: ResourceAdmissionRequest<'_, '_, E, D, B>,
    coverage: &mut WalkCoverage,
) -> ResourceAdmissionOutcome<B>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let ResourceAdmissionRequest {
        resource,
        parsed,
        schema,
        quoted,
        inputs,
        evaluation,
        bindings,
        session,
    } = request;
    let (values, classification_diagnostics) =
        with_classified_inputs(&inputs, evaluation, session, |inputs, attributes| {
            if let Err(missing) = inputs.authored_evidence() {
                coverage.no_authored_evidence(missing);
            }
            let missing = inputs.prepare_missing::<Pass>(inputs.report(parsed.expression_report()));
            let visits = std::cell::RefCell::new(std::mem::take(coverage));
            let findings = if uses_resource_attribute_admission(resource.kind()) {
                typed_resource_attribute_admissions::<Pass, kind::ResourceAttribute, _, _, _>(
                    resource, parsed, schema, attributes, quoted, inputs, bindings, session,
                    &visits,
                )
            } else {
                typed_resource_attribute_admissions::<Pass, kind::DataSourceAttribute, _, _, _>(
                    resource, parsed, schema, attributes, quoted, inputs, bindings, session,
                    &visits,
                )
            };
            *coverage = visits.into_inner();
            findings.map_value(|findings| missing.finish(findings, coverage))
        });
    ResourceAdmissionOutcome {
        values,
        classification_diagnostics,
    }
}

pub(crate) fn resource_attribute_admissions<E, D, B: BindingAddress>(
    request: ResourceAdmissionRequest<'_, '_, E, D, B>,
    coverage: &mut WalkCoverage,
) -> ResourceAdmissionOutcome<B>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resource_admissions_for_pass::<LiveResource, _, _, _>(request, coverage)
}

/// Admit every analysis item using the same destination-kind policy.
/// Classifications are built from the consumed inputs.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn resource_attribute_admissions_for_test<
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
    E,
>(
    resource: &crate::parser::AnalysisResourceRef<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    parsed: &crate::parser::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    quoted: &std::collections::HashSet<String>,
    inputs: ResourceAttributeInputs<B>,
    evaluation: ValidationEvaluation<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
    coverage: &mut WalkCoverage,
) -> ResourceAdmissionOutcome<B> {
    resource_admissions_for_pass::<LiveResource, _, _, _>(
        ResourceAdmissionRequest {
            resource,
            parsed: crate::identifier::PrefixFileRead::for_test(parsed, schemas),
            schema,
            quoted,
            inputs,
            evaluation,
            bindings,
            session,
        },
        coverage,
    )
}

/// Retained expressions supply computed-access and arithmetic evidence to admit.
#[allow(clippy::too_many_arguments)]
fn typed_resource_attribute_admissions<
    Pass: ResourceAdmissionPass,
    K: RechecksResolved,
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            B,
        >,
    E,
>(
    resource: &crate::parser::AnalysisResourceRef<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    parsed: crate::identifier::PrefixFileRead<'_, E, ParserPlaceholder, ParserPlaceholder, D, B>,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &indexmap::IndexMap<
        String,
        crate::resource::knowledge::Classification<'_, crate::resource::ParserPlaceholder, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    inputs: &ResourceAttributeInputs<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    coverage: &std::cell::RefCell<crate::schema::value_validation::WalkCoverage>,
) -> crate::identifier::PrefixAdmitted<crate::schema::ResourceValueValidation> {
    use crate::resource::{DeferredValue, ParserPlaceholder};
    let report = inputs.report(parsed.expression_report());
    let attribute_input = |name: &str| {
        inputs.input(
            resource,
            name,
            report,
            schema
                .schema()
                .as_schema()
                .attributes()
                .get(name)
                .and_then(|field| field.parts().block_name),
        )
    };
    let mut declaration_errors = std::collections::HashMap::new();
    let declarations = attributes
        .iter()
        .map(|(name, proof)| {
            let expression = match attribute_input(name).map_err(|error| (name, error))? {
                AttributeInput::Authored { slot } => slot
                    .cursor()
                    .map_err(|error| (name, error))?
                    .node()
                    .judgement(),
                AttributeInput::Reported { .. } => match report.expression(
                    resource.attribute_value_location(name, &crate::schema::FieldPath::new()),
                ) {
                    Ok(expression) => expression,
                    // Main's report path treats absent declaration recovery as
                    // no lexical declaration. Ordinary expression checks below
                    // still own their original lookup errors.
                    Err(_) => return Ok(None),
                },
                AttributeInput::Projected { .. } => return Ok(None),
            };
            let value = expression.assignment_value(
                crate::resource::knowledge::ClassifiedRef::from(proof).diagnostic_value(),
            );
            Ok(parsed
                .module_resource_ref_declaration(resource.id(), name, value, bindings)
                .map(|(source_name, source)| (name.clone(), (source_name.to_owned(), source))))
        })
        .filter_map(|prepared| match prepared {
            Ok(declaration) => declaration,
            Err((name, error)) => {
                declaration_errors.insert(name.clone(), error);
                None
            }
        })
        .collect();
    let check = |name: &str,
                 attr: &CompiledAttributeType,
                 value: &Value<ParserPlaceholder, B>,
                 _path: &LeafPath,
                 contract: &DestinationContract,
                 boundary: &assignment::ReferenceContext<'_, B>,
                 cursor: crate::parser::retained::AuthoredCursor<'_, B>| {
        if let Some(error) = declaration_errors.get(name) {
            return Err(assignment_seam_error(error.clone()));
        }
        match value {
            Value::Deferred(
                DeferredValue::Interpolation(_)
                | DeferredValue::Access(_)
                | DeferredValue::Coalesce { .. }
                | DeferredValue::IntegerArithmetic(_)
                | DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument { .. }),
            ) => {}
            Value::Concrete(_)
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::ModuleIf(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::Secret(_)
                | DeferredValue::Unknown(_)
                | DeferredValue::Placeholder(_),
            ) => return Ok(Default::default()),
        }
        enum Evidence<'a, B: BindingAddress> {
            Node(crate::parser::retained::JudgedNodeRef<'a, B>),
            Report(&'a crate::parser::retained::ExpressionReport<B>),
        }
        let evidence = match &cursor.kind {
            crate::parser::retained::CursorKind::Authored(usage)
            | crate::parser::retained::CursorKind::CoveredBy(usage) => {
                Evidence::Node(usage.node().clone())
            }
            crate::parser::retained::CursorKind::Projected(_) if inputs.is_reported(name) => {
                Evidence::Report(report)
            }
            crate::parser::retained::CursorKind::Projected(_) => return Ok(Default::default()),
        };
        let expression = match &evidence {
            Evidence::Node(node) => node.judgement(),
            Evidence::Report(report) => {
                let relative = _path.segments().iter().skip(1).fold(
                    crate::schema::FieldPath::new(),
                    |path, step| match step {
                        LeafPathSegment::StructField(name) => path.push_field(name),
                        LeafPathSegment::ListIndex(index) => path.push_index(*index),
                        LeafPathSegment::MapKey(key) => path.push_map_key(key.expose_plaintext()),
                    },
                );
                report
                    .expression(resource.attribute_value_location(name, &relative))
                    .or_else(|_| {
                        report.expression(
                            resource
                                .attribute_value_location(name, &crate::schema::FieldPath::new()),
                        )
                    })
                    .map_err(assignment_seam_error)?
            }
        };
        let sink = attr
            .in_schema(schema.schema().as_schema().defs())
            .with_authority(
                &session.catalog().custom_types,
                crate::schema::SinkAuthority::Owner(schema.schema_type_owner()),
            );
        let judge_argument = |value| {
            let reference = crate::resource::module_argument_reference_path(value)?;
            let reference = ReferenceCandidate { reference };
            let mut findings = Default::default();
            Some(
                reference
                    .judge_at::<K>(contract, attr, _path, boundary, &mut findings)
                    .map(|()| findings)
                    .map_err(|error| reference.error_with_origin(error)),
            )
        };
        if let Value::Deferred(DeferredValue::Interpolation(_)) = value {
            let interpolation_sink =
                crate::validation::lift_type_expr(&crate::validation::INTERPOLATION_REF_SINK)
                    .expect("built-in interpolation sink");
            let context = crate::validation::ResourceReferenceContext {
                location: crate::validation::ReferenceFindingLocation::Resource {
                    location: resource.id().to_string(),
                    attribute: name.into(),
                },
                bindings,
                declared_source: None,
                interpolation_sink: &interpolation_sink,
            };
            let errors = crate::validation::collect_ref_diagnostics_with_sink(
                value,
                None,
                &mut |reference, sink, usage, _, errors| {
                    errors.extend(crate::validation::check_resource_reference(
                        &context, reference, sink, usage,
                    ));
                },
            );
            let computed = crate::validation::visit_computed_accesses_with_sink(
                value,
                None,
                &mut |access, sink| {
                    let Some(crate::validation::RefSink::Interpolation) = sink else {
                        return Vec::new();
                    };
                    let expression = match access {
                        Value::Deferred(DeferredValue::Access(access)) => {
                            let access_evidence = match &evidence {
                                Evidence::Node(authored) => access
                                    .access_locations()
                                    .ok_or_else(|| {
                                        crate::parser::retained::MissingEvidence::new(
                                            "computed access identity",
                                        )
                                    })
                                    .and_then(|locations| locations.node_id())
                                    .and_then(|id| authored.retained_node(id))
                                    .map(|node| node.judgement()),
                                Evidence::Report(report) => report.access(access),
                            };
                            match access_evidence {
                                Ok(expression) => expression,
                                Err(error) => {
                                    return vec![
                                        crate::validation::DeferredAccessTypeError::InvalidAccess {
                                            error: error.into(),
                                        },
                                    ];
                                }
                            }
                        }
                        Value::Concrete(_) | Value::Deferred(_) => expression,
                    };
                    crate::validation::check_deferred_access_type(
                        access,
                        interpolation_sink.assignment_sink(&session.catalog().custom_types),
                        expression,
                        bindings,
                    )
                    .err()
                    .into_iter()
                    .collect()
                },
            );
            let errors = errors
                .into_iter()
                .map(|finding| {
                    assignment::reference_error(AdmissionReferenceError::Authored(Box::new(
                        finding,
                    )))
                })
                .chain(computed.into_iter().map(|error| {
                    assignment::reference_error(AdmissionReferenceError::Computed(error))
                }))
                .collect::<Vec<_>>();
            if !errors.is_empty() {
                let path = LeafPath::from_segments(_path.segments().iter().skip(1).cloned());
                return Ok(crate::schema::value_validation::AssignmentFindings {
                    errors: errors
                        .into_iter()
                        .map(|error| (path.clone(), error))
                        .collect(),
                    ..Default::default()
                });
            }
            if let Some(judged) = judge_argument(expression.assignment_value(value)) {
                return judged;
            }
            if let assignment::CandidateOrigin::Reference(reference) =
                assignment::CandidateOrigin::of(expression.assignment_value(value))
            {
                let mut findings = Default::default();
                reference.judge_at::<K>(contract, attr, _path, boundary, &mut findings)?;
                return Ok(findings);
            }
        }
        match value {
            Value::Deferred(DeferredValue::Access(_) | DeferredValue::Coalesce { .. }) => {
                crate::validation::check_deferred_access_type(value, sink, expression, bindings)
                    .map_err(|error| {
                        assignment::reference_error(AdmissionReferenceError::Computed(error))
                    })?;
            }
            Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
                let operands: &dyn crate::validation::IntegerOperandEvidence<B> = match &evidence {
                    Evidence::Node(node) => node,
                    Evidence::Report(report) => *report,
                };
                if let Some(finding) = crate::validation::check_integer_arithmetic_types_with_sink(
                    value,
                    Some(crate::validation::RefSink::EffectiveType {
                        attr_type: attr,
                        defs: schema.schema().as_schema().defs(),
                        owner: schema.schema_type_owner(),
                        ancestry: FieldAncestry::WRITABLE,
                    }),
                    operands,
                    bindings,
                )
                .into_iter()
                .next()
                {
                    return Err(assignment::reference_error(
                        AdmissionReferenceError::Arithmetic(finding),
                    ));
                }
            }
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument {
                ..
            })) => {
                if let Some(judged) = judge_argument(value) {
                    return judged;
                }
            }
            Value::Concrete(_)
            | Value::Deferred(
                DeferredValue::ResourceRef { .. }
                | DeferredValue::BindingRef { .. }
                | DeferredValue::ModuleIf(_)
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall { .. }
                | DeferredValue::ResolvedFunctionCall { .. }
                | DeferredValue::Unavailable { .. }
                | DeferredValue::Secret(_)
                | DeferredValue::Unknown(_)
                | DeferredValue::Placeholder(_),
            ) => {}
        }
        Ok(Default::default())
    };
    let candidate = |name: &str, proof| {
        if let Some(error) = declaration_errors.get(name) {
            return Err(seam_constraint(error.clone()));
        }
        let (candidate, mut boundary) = match attribute_input(name).map_err(seam_constraint)? {
            AttributeInput::Authored { slot, .. } => {
                Pass::candidate(slot, proof, bindings, session)
            }
            AttributeInput::Projected { phase, .. } => Ok(Candidate::validation_projected(
                proof,
                Some(bindings),
                session,
                phase,
            )),
            AttributeInput::Reported { source } => Ok(Candidate::validation_projected(
                proof,
                Some(bindings),
                session,
                crate::parser::retained::ValidationProjection::judged(source),
            )),
        }?;
        boundary.scope = inputs.scope;
        Ok(Box::new((candidate, boundary)))
    };
    let input = ValidationInput::Resource {
        candidate: &candidate,
        coverage,
    };
    admit_validation_attributes_with_input::<K, _, _>(
        schema,
        resource.id(),
        attributes,
        inputs::in_resource_order(resource, attributes),
        quoted,
        Some(bindings),
        session,
        Some(&check),
        Some(&declarations),
        inputs.awaiting_prefixes(),
        input,
    )
}

/// Complete authored conversion evidence for a sibling file whose ordinary
/// diagnostics are outside the current LSP document. Each resource is visited
/// once; ordinary/provider callbacks retain the main-branch document scope.
pub(crate) fn unrendered_resource_conversion_admissions<E, D, B: BindingAddress>(
    resource: &crate::identifier::CanonicalResource<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    prechecks: SlotPrechecks<'_>,
    coverage: &mut WalkCoverage,
) -> Result<(), crate::parser::retained::MissingEvidence>
where
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    unrendered_resource_admissions::<LiveResource, _, _, _>(
        resource, bindings, session, prechecks, coverage,
    )
}
fn unrendered_resource_admissions<Pass: ResourceAdmissionPass, E, D, B: BindingAddress>(
    resource: &crate::identifier::CanonicalResource<'_, E, D, B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    prechecks: SlotPrechecks<'_>,
    coverage: &mut WalkCoverage,
) -> Result<(), crate::parser::retained::MissingEvidence>
where
    E: crate::parser::ExportParamLike<ParserPlaceholder, B>,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let values = resource.values();
    let mut inputs = ResourceAttributeInputs::for_resource(resource, resource.report())
        .unwrap_or_else(|_missing| {
            ResourceAttributeInputs::from_report(resource, &values, resource.report())
        })
        .with_prechecks(prechecks);
    inputs.scope = crate::schema::value_validation::AssignmentScope::ClaimedValues;
    let request = ResourceAdmissionRequest::new(
        resource,
        resource.quoted_string_attrs(),
        inputs,
        ValidationEvaluation::None,
        bindings,
        session,
    )?;
    resource_admissions_for_pass::<Pass, _, _, _>(request, coverage);
    Ok(())
}

#[cfg(doctest)]
/// The paired examples differ only in the argument supplied as the product.
///
/// ```
/// use carina_core::{binding_index::BindingIndex,
///     identifier::CanonicalResource, parser::{ParsedExportParam, ParsedDeferredForPhase},
///     resource::{ParserPlaceholder, RelativeBindingName}, resource::knowledge::Classification,
///     validation::admission::attributes::{ResourceAdmissionRequest, ResourceAttributeInputs, ValidationEvaluation},
///     validation::deferred_types::ProviderTypeCheckContext};
/// type Owner<'f> = CanonicalResource<'f, ParsedExportParam,
///     ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>, RelativeBindingName>;
/// fn caller<'a, 'file: 'a>(resource: &'a Owner<'file>,
///     classifications: &indexmap::IndexMap<String, Classification<'a, ParserPlaceholder, RelativeBindingName>>,
///     quoted: &'a std::collections::HashSet<String>, inputs: ResourceAttributeInputs<RelativeBindingName>,
///     evaluation: ValidationEvaluation<RelativeBindingName>, bindings: &'a BindingIndex<'a, RelativeBindingName>,
///     session: &'a dyn ProviderTypeCheckContext) {
///     let _ = ResourceAdmissionRequest::new(resource, quoted,
///         inputs, evaluation, bindings, session);
/// }
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::{binding_index::BindingIndex,
///     identifier::CanonicalResource, parser::{ParsedExportParam, ParsedDeferredForPhase},
///     resource::{ParserPlaceholder, RelativeBindingName}, resource::knowledge::Classification,
///     validation::admission::attributes::{ResourceAdmissionRequest, ResourceAttributeInputs, ValidationEvaluation},
///     validation::deferred_types::ProviderTypeCheckContext};
/// type Owner<'f> = CanonicalResource<'f, ParsedExportParam,
///     ParsedDeferredForPhase<ParserPlaceholder, RelativeBindingName>, RelativeBindingName>;
/// fn caller<'a, 'file: 'a>(resource: &'a Owner<'file>,
///     classifications: &indexmap::IndexMap<String, Classification<'a, ParserPlaceholder, RelativeBindingName>>,
///     quoted: &'a std::collections::HashSet<String>, inputs: ResourceAttributeInputs<RelativeBindingName>,
///     evaluation: ValidationEvaluation<RelativeBindingName>, bindings: &'a BindingIndex<'a, RelativeBindingName>,
///     session: &'a dyn ProviderTypeCheckContext) {
///     let _ = ResourceAdmissionRequest::new(resource, quoted,
///         classifications, evaluation, bindings, session);
/// }
/// ```
///
/// A slot's classification and cursor are paired inside admission.
///
/// ```
/// use carina_core::{binding_index::BindingIndex, parser::retained::JudgedSlot,
///     resource::{ParserPlaceholder, RelativeBindingName}, resource::knowledge::Classification,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(slot: &'a JudgedSlot<RelativeBindingName>,
///     proof: Classification<'a, ParserPlaceholder, RelativeBindingName>,
///     bindings: Option<&'a BindingIndex<'a, RelativeBindingName>>,
///     provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate<ParserPlaceholder, RelativeBindingName>> = None;
///     let _ = (slot, proof, bindings, provider);
/// }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::{binding_index::BindingIndex, parser::retained::JudgedSlot,
///     resource::{ParserPlaceholder, RelativeBindingName}, resource::knowledge::Classification,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(slot: &'a JudgedSlot<RelativeBindingName>,
///     proof: Classification<'a, ParserPlaceholder, RelativeBindingName>,
///     bindings: Option<&'a BindingIndex<'a, RelativeBindingName>>,
///     provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate<ParserPlaceholder, RelativeBindingName>> = None;
///     let _ = Candidate::validation_slot(slot, proof, bindings, provider);
/// }
/// ```
///
/// Raw admission entry and candidate factories belong to admission. These
/// external-crate examples pin their lack of public visibility; plain private
/// declarations also exclude sibling modules inside this crate.
/// The classified factory is additionally absent from non-test builds.
///
/// ```
/// use carina_core::validation::admission::Candidate;
/// ```
///
/// ```compile_fail,E0603
/// use carina_core::validation::admission::admit;
/// ```
///
/// ```
/// use carina_core::{binding_index::BindingIndex, parser::retained::NoConversion,
///     resource::AbsoluteBindingAddress, resource::knowledge::Classification,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: Classification<'a>,
///     bindings: Option<&'a BindingIndex<'a, AbsoluteBindingAddress>>,
///     provider: &'a dyn ProviderTypeCheckContext, no_conversion: NoConversion) {
///     let _: Option<Candidate> = None;
///     let _ = (proof, bindings, provider, no_conversion);
/// }
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::{binding_index::BindingIndex, parser::retained::NoConversion,
///     resource::AbsoluteBindingAddress, resource::knowledge::Classification,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: Classification<'a>,
///     bindings: Option<&'a BindingIndex<'a, AbsoluteBindingAddress>>,
///     provider: &'a dyn ProviderTypeCheckContext, no_conversion: NoConversion) {
///     let _: Option<Candidate> = None;
///     let _ = Candidate::validation_classified(proof, bindings, provider, no_conversion);
/// }
/// ```
///
/// ```
/// use carina_core::{resource::knowledge::PlanResolved,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: PlanResolved<'a>, provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate> = None;
///     let _ = (proof, provider);
/// }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::{resource::knowledge::PlanResolved,
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: PlanResolved<'a>, provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate> = None;
///     let _ = Candidate::plan(proof, provider);
/// }
/// ```
///
/// ```
/// use carina_core::{resource::{AbsoluteBindingAddress, NoPlaceholder},
///     resource::knowledge::{Apply, Knowledge},
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: Knowledge<'a, NoPlaceholder, AbsoluteBindingAddress, Apply>,
///     provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate> = None;
///     let _ = (proof, provider);
/// }
/// ```
///
/// ```compile_fail,E0624
/// use carina_core::{resource::{AbsoluteBindingAddress, NoPlaceholder},
///     resource::knowledge::{Apply, Knowledge},
///     validation::{admission::Candidate, deferred_types::ProviderTypeCheckContext}};
/// fn paired<'a>(proof: Knowledge<'a, NoPlaceholder, AbsoluteBindingAddress, Apply>,
///     provider: &'a dyn ProviderTypeCheckContext) {
///     let _: Option<Candidate> = None;
///     let _ = Candidate::apply(proof, provider);
/// }
/// ```
fn resource_attribute_admission_api_pins() {}

#[cfg(test)]
mod review_tests {
    use super::*;
    use crate::schema::SchemaKind;

    #[test]
    fn apply_admission_validates_every_supplied_attribute() {
        let resource: crate::resource::Resource<_, NoPlaceholder, AbsoluteBindingAddress> =
            crate::resource::Resource::with_provider("test", "Sink", "sink", None);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            crate::schema::ResourceSchema::new("Sink")
                .attribute(
                    crate::schema::AttributeSchema::optional(
                        "extra",
                        crate::schema::AttributeType::string(),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let attributes = indexmap::IndexMap::from([(
            "extra".into(),
            Value::Concrete(crate::resource::ConcreteValue::Int(1)),
        )]);
        let catalog = crate::parser::LoadedProviderContext::empty_for_test();
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            let result = admit_apply_attributes::<kind::ResourceAttribute>(
                &resource.id,
                &attributes,
                None,
                &Default::default(),
                schemas.get_for(&resource).unwrap(),
                &ResolvedBindings::default(),
                session,
            );
            let Err(ApplyAdmissionErrors::Constraints(report)) = result else {
                panic!("extra attribute must be validated");
            };
            assert_eq!(
                report
                    .diagnostic_errors()
                    .map(|(diagnostic, _)| diagnostic.path().unwrap().0)
                    .collect::<Vec<_>>(),
                ["extra"]
            );
        });
    }

    #[test]
    fn review_r2_validation_and_plan_keep_resolution_failures_without_findings() {
        let failure = || {
            Err::<Admission<kind::ResourceAttribute>, _>(AdmissionErrors::from(
                ResolutionError::Message("reference resolution failed".into()),
            ))
        };
        let validated = admission_findings(&mut failure());
        let planned = admission_findings(&mut failure());
        assert_eq!(validated, planned);
        assert!(matches!(
            validated.local_error(),
            Some(TypeError::Reference { .. })
        ));
    }

    #[test]
    fn review_r6_apply_reports_unresolved_and_constraint_failures_together() {
        let catalog = crate::parser::LoadedProviderContext::empty_for_test();
        let mut schemas = SchemaRegistry::new();
        let mut schema = crate::schema::ResourceSchema::new("Sink");
        for name in ["first", "second", "third", "fourth"] {
            schema = schema.attribute(
                crate::schema::AttributeSchema::optional(
                    name,
                    crate::schema::AttributeType::string(),
                )
                .build(),
            );
        }
        schemas.insert("test", schema.validate().unwrap());
        let resource = crate::resource::Resource::with_provider("test", "Sink", "sink", None)
            .with_attribute("first", Value::root_resource_ref("a", "output", vec![]))
            .with_attribute(
                "second",
                Value::Concrete(crate::resource::ConcreteValue::Int(1)),
            )
            .with_attribute("third", Value::root_resource_ref("b", "output", vec![]))
            .with_attribute(
                "fourth",
                Value::Concrete(crate::resource::ConcreteValue::Int(2)),
            );
        crate::validation::deferred_types::with_session_for_test(
            catalog.as_provider_context(),
            |session| {
                let result = admit_apply_resource(
                    &resource,
                    None,
                    schemas.get_for(&resource).unwrap(),
                    &ResolvedBindings::default(),
                    session,
                );
                let errors = match result {
                    Err(ApplyAdmissionErrors::Constraints(report)) => report
                        .diagnostic_errors()
                        .map(|(diagnostic, _)| {
                            format!("{}: {}", diagnostic.path().unwrap().0, diagnostic.error())
                        })
                        .collect::<Vec<_>>(),
                    Err(ApplyAdmissionErrors::Admission(error)) => {
                        vec![error.resolution_error().to_string()]
                    }
                    Ok(_) => panic!("invalid attributes were admitted"),
                };
                assert_eq!(errors.len(), 4, "{errors:#?}");
                for name in ["first", "second", "third", "fourth"] {
                    assert!(
                        errors.iter().any(|error| error.contains(name)),
                        "missing {name}: {errors:#?}"
                    );
                }
            },
        );
    }

    #[test]
    fn review_r8_owner_identity_is_independent_of_other_loaded_providers() {
        let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            crate::schema::ResourceSchema::new("Sink")
                .validate()
                .unwrap(),
        );
        let id = ResourceId::with_provider_identity("test", "Sink", "sink", None);
        let before = identity(
            schemas.get("test", "Sink", SchemaKind::Resource).unwrap(),
            &id,
            &catalog,
        )
        .declaration
        .owner;
        catalog
            .resource_types
            .insert(("aaa".into(), "Other".into()));
        schemas.insert(
            "aaa",
            crate::schema::ResourceSchema::new("Other")
                .validate()
                .unwrap(),
        );
        let after = identity(
            schemas.get("test", "Sink", SchemaKind::Resource).unwrap(),
            &id,
            &catalog,
        )
        .declaration
        .owner;
        assert_eq!(before, after);
    }
}
