//! Catalog-bound resource and data-source attribute descriptors.

use super::*;
use crate::resource::ResourceId;
use crate::schema::{LeafPathSegment, SchemaKind, SchemaRegistry};

mod inputs;
use inputs::AttributeInput;
pub use inputs::ResourceAttributeInputs;

/// A catalog descriptor requires a registered schema owner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DestinationError {
    #[error("unknown schema owner '{provider}'")]
    UnknownOwner { provider: String },
    #[error("schema unavailable for {resource}")]
    SchemaUnavailable { resource: Box<ResourceId> },
    #[error("unknown attribute '{attribute}' on {resource}")]
    UnknownAttribute {
        resource: Box<ResourceId>,
        attribute: String,
    },
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

impl SchemaRegistry {
    fn admission_attribute<K: DestinationKind>(
        &self,
        id: &ResourceId,
        attribute: &str,
        kind: SchemaKind,
        catalog: &ProviderContext,
    ) -> Result<Destination<K>, DestinationError> {
        let schema = self
            .get(&id.provider, &id.resource_type, kind)
            .ok_or_else(|| {
                if self.assignability_closures().contains_key(
                    &crate::parser::SchemaTypeOwner::Provider(id.provider.clone()),
                ) {
                    DestinationError::SchemaUnavailable {
                        resource: Box::new(id.clone()),
                    }
                } else {
                    DestinationError::UnknownOwner {
                        provider: id.provider.clone(),
                    }
                }
            })?;
        schema
            .admission_attribute(id, attribute, catalog)
            .ok_or_else(|| DestinationError::UnknownAttribute {
                resource: Box::new(id.clone()),
                attribute: attribute.into(),
            })
    }

    pub fn resource_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Result<Destination<kind::ResourceAttribute>, DestinationError> {
        self.admission_attribute(id, attribute, SchemaKind::Resource, catalog)
    }

    pub fn data_source_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Result<Destination<kind::DataSourceAttribute>, DestinationError> {
        self.admission_attribute(id, attribute, SchemaKind::DataSource, catalog)
    }
}

impl crate::schema::OwnerBoundResourceSchema<'_> {
    fn untyped_attribute<K: DestinationKind>(
        self,
        id: &ResourceId,
        name: &str,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        Destination::catalogued(identity(self, id, catalog), name.into())
    }

    pub(crate) fn admission_attribute<K: DestinationKind>(
        self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Option<Destination<K>> {
        let effective = self.effective_attribute_by_authored_name(attribute)?;
        let ordinal = self
            .schema()
            .as_schema()
            .attributes()
            .keys()
            .position(|name| name == effective.name())?;
        Some(self.admission_selected_attribute(
            id,
            attribute,
            crate::schema::value_validation::SchemaAttribute { ordinal, effective },
            catalog,
        ))
    }

    fn admission_selected_attribute<K: DestinationKind>(
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

fn admission_findings<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
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
    resource: &crate::parser::TopLevelResourceRef<'_>,
    attributes: &indexmap::IndexMap<String, PlanResolved<'_>>,
    quoted: &std::collections::HashSet<String>,
    session: &dyn ProviderTypeCheckContext,
) -> crate::schema::ResourceValueValidation {
    let projected = resource
        .attributes()
        .keys()
        .filter_map(|name| {
            attributes
                .get(name)
                .map(|proof| (name.clone(), proof.knowledge().clone()))
        })
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
                    resource.id(),
                    name,
                    selected,
                    session.catalog(),
                );
                let (candidate, boundary) = Candidate::plan(attributes[name].clone(), session);
                admission_findings(&mut admit(&destination, candidate, &boundary))
            },
        )
}

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

/// One authored attribute report, shared by CLI validation and LSP rendering.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_validation_attributes<
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    attributes: &indexmap::IndexMap<String, crate::resource::knowledge::Classification<'_, P, B>>,
    quoted: &std::collections::HashSet<String>,
    bindings: Option<&BindingIndex<'_, B>>,
    session: &dyn ProviderTypeCheckContext,
    check: ValidationAssignmentCheck<'_, P, B>,
    declarations: Option<
        &std::collections::HashMap<String, (String, crate::binding_index::DeclaredValueSource)>,
    >,
) -> crate::schema::ResourceValueValidation {
    admit_validation_attributes_with_input::<K, P, B>(
        schema,
        id,
        attributes,
        quoted,
        bindings,
        session,
        check,
        declarations,
        ValidationInput::Classified,
    )
}

type ValidationCandidateFactory<'a, P, B> =
    &'a dyn Fn(
        &str,
        crate::resource::knowledge::Classification<'a, P, B>,
    ) -> Result<(Candidate<P, B>, AdmissionContext<'a, P, B>), AdmissionErrors>;

/// The classified path is for explicit schema-only producers. A live authored
/// source must supply its cursor factory; association failure is never projected.
enum ValidationInput<'a, P: PlaceholderKind, B: BindingAddress> {
    Classified,
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
    'v,
    K: RechecksResolved,
    P: PlaceholderKind,
    B: BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    attributes: &indexmap::IndexMap<String, crate::resource::knowledge::Classification<'v, P, B>>,
    quoted: &std::collections::HashSet<String>,
    bindings: Option<&BindingIndex<'_, B>>,
    session: &dyn ProviderTypeCheckContext,
    check: ValidationAssignmentCheck<'_, P, B>,
    declarations: Option<
        &std::collections::HashMap<String, (String, crate::binding_index::DeclaredValueSource)>,
    >,
    input: ValidationInput<'v, P, B>,
) -> crate::schema::ResourceValueValidation {
    let admit_attribute = |name: &str,
                           proof: &crate::resource::knowledge::Classification<'v, P, B>,
                           destination: &Destination<K>| {
        let (candidate, mut boundary) = match &input {
            ValidationInput::Classified => {
                Candidate::validation_classified(proof.clone(), bindings, session)
            }
            ValidationInput::Resource { candidate, .. } => match candidate(name, proof.clone()) {
                Ok(input) => input,
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
                authored: None,
                ordinary: assignment.as_ref().map(|check| check as &_),
            },
            &untyped,
        );
        let findings = admission_findings(&mut admit(destination, candidate, &boundary));
        if let ValidationInput::Resource { coverage, .. } = &input {
            coverage.borrow_mut().extend(boundary.coverage.take());
        }
        if proof.failure().is_some() {
            // Keep attempted failure evidence and attribute presence. The
            // expression/collector pass owns its validation diagnostic.
            findings.retained_failure()
        } else {
            findings
        }
    };
    schema.schema().as_schema().validate_admission_report(
        attributes,
        &|name| quoted.contains(name),
        &crate::schema::value_validation::AttributeAdmission {
            known: &|name, selected, proof| {
                let destination =
                    schema.admission_selected_attribute::<K>(id, name, selected, session.catalog());
                admit_attribute(name, proof, &destination)
            },
            unknown: Some(&|name, proof| {
                let destination = schema.untyped_attribute::<K>(id, name, session.catalog());
                admit_attribute(name, proof, &destination)
            }),
        },
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

/// Admit every analysis item using the same destination-kind policy.
#[allow(clippy::too_many_arguments)]
pub fn resource_attribute_admissions<
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
    attributes: &indexmap::IndexMap<
        String,
        crate::resource::knowledge::Classification<'_, crate::resource::ParserPlaceholder, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    inputs: &ResourceAttributeInputs<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
    coverage: &mut WalkCoverage,
) -> crate::schema::ResourceValueValidation {
    if let Err(missing) = inputs.authored_evidence() {
        coverage.no_authored_evidence(missing);
    }
    let attributes = inputs::in_resource_order(resource, attributes)
        .map(|(name, proof)| (name.clone(), proof.clone()))
        .collect();
    let visits = std::cell::RefCell::new(std::mem::take(coverage));
    let findings = if uses_resource_attribute_admission(resource.kind()) {
        typed_resource_attribute_admissions::<kind::ResourceAttribute, _, _, _>(
            resource,
            parsed,
            schema,
            &attributes,
            quoted,
            inputs,
            bindings,
            session,
            schemas,
            &visits,
        )
    } else {
        typed_resource_attribute_admissions::<kind::DataSourceAttribute, _, _, _>(
            resource,
            parsed,
            schema,
            &attributes,
            quoted,
            inputs,
            bindings,
            session,
            schemas,
            &visits,
        )
    };
    *coverage = visits.into_inner();
    findings
}

/// Retained expressions supply computed-access and arithmetic evidence to admit.
#[allow(clippy::too_many_arguments)]
fn typed_resource_attribute_admissions<
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
    parsed: &crate::parser::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        D,
        B,
    >,
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &indexmap::IndexMap<
        String,
        crate::resource::knowledge::Classification<'_, crate::resource::ParserPlaceholder, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    inputs: &ResourceAttributeInputs<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
    coverage: &std::cell::RefCell<crate::schema::value_validation::WalkCoverage>,
) -> crate::schema::ResourceValueValidation {
    use crate::resource::{DeferredValue, ParserPlaceholder};
    let report = inputs.report(parsed.expression_report());
    let mut declaration_errors = std::collections::HashMap::new();
    let declarations = attributes
        .iter()
        .map(|(name, proof)| {
            let expression = match inputs
                .input(resource, name)
                .map_err(|error| (name, error))?
            {
                AttributeInput::Authored { slot } => slot
                    .cursor()
                    .map_err(|error| (name, error))?
                    .node()
                    .judgement(),
                AttributeInput::Reported => match report.expression(
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
            Ok(crate::validation::module_resource_ref_declaration(
                parsed,
                resource.id(),
                name,
                value,
                bindings,
            )
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
                        schemas,
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
                crate::validation::check_deferred_access_type(
                    value, sink, expression, schemas, bindings,
                )
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
        match inputs.input(resource, name).map_err(seam_constraint)? {
            AttributeInput::Authored { slot, .. } => {
                Candidate::validation_slot(slot, proof, Some(bindings), session)
            }
            AttributeInput::Projected { phase, .. } => Ok(Candidate::validation_projected(
                proof,
                Some(bindings),
                session,
                phase,
            )),
            AttributeInput::Reported => Ok(Candidate::validation_classified(
                proof,
                Some(bindings),
                session,
            )),
        }
    };
    let input = ValidationInput::Resource {
        candidate: &candidate,
        coverage,
    };
    admit_validation_attributes_with_input::<K, _, _>(
        schema,
        resource.id(),
        attributes,
        quoted,
        Some(bindings),
        session,
        Some(&check),
        Some(&declarations),
        input,
    )
}

#[cfg(test)]
mod review_tests {
    use super::*;

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
                        .errors_with_paths()
                        .map(|(error, key)| format!("{}: {error}", key.unwrap().0))
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
        let unknown = ResourceId::with_provider_identity("unknown", "Sink", "sink", None);
        assert!(matches!(
            schemas.resource_attribute_destination(&unknown, "value", &catalog),
            Err(DestinationError::UnknownOwner { .. })
        ));
    }
}
