//! Catalog-bound resource and data-source attribute descriptors.

use super::*;
use crate::resource::ResourceId;
use crate::schema::{LeafPathSegment, SchemaKind, SchemaRegistry};

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
            owner: schema.schema_type_owner().clone(),
            schema: schema.schema().admission_identity,
        },
        instance: InstanceIdentity::Resource(id.clone()),
        path: vec![],
        revision: CatalogRevision(catalog.custom_types.clone()),
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
        let mut destination =
            Destination::catalogued(identity(self, id, catalog), attribute.into());
        destination.identity.path.push(ordinal);
        destination.contract = Some(Box::new(DestinationContract {
            attr: effective.field_type().clone(),
            defs: self.schema().as_schema().defs.clone(),
            owner: self.schema_type_owner().clone(),
            ancestry: FieldAncestry::from_input_mode(effective.effective_input_mode()),
            path: LeafPath::from_segments([LeafPathSegment::StructField(attribute.into())]),
        }));
        Some(destination)
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

/// Plan uses only the plan-stage proof. Apply-pending sites remain in each admission.
pub fn admit_plan_attributes<K: RechecksResolved>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    id: &ResourceId,
    attributes: &std::collections::HashMap<String, PlanResolved<'_>>,
    quoted: &std::collections::HashSet<String>,
    session: &dyn ProviderTypeCheckContext,
) -> crate::schema::ResourceValueValidation {
    let projected = attributes
        .iter()
        .map(|(name, proof)| (name.clone(), proof.knowledge().clone()))
        .collect();
    let lookup = crate::parser::custom_type_lookup_for_schema(
        session.catalog().custom_type_resolver(),
        schema,
    );
    schema
        .schema()
        .as_schema()
        .validate_resolved_admission_report(
            &projected,
            None,
            &|name| quoted.contains(name),
            &lookup,
            &|name, _| {
                let destination = schema
                    .admission_attribute::<K>(id, name, session.catalog())
                    .expect("schema selected attribute");
                let (candidate, boundary) = Candidate::plan(attributes[name].clone(), session);
                admission_findings(&mut admit(&destination, candidate, &boundary))
            },
        )
}

/// Minted only after every selected attribute has produced a placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmittedAttributes<K: RechecksResolved> {
    kind: PhantomData<K>,
}

pub(crate) enum ApplyAdmissionErrors {
    Admission(AdmissionErrors),
    Constraints(crate::schema::ResourceValueValidation),
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
    resolver: &dyn crate::parser::CustomTypeResolver,
) -> Result<AdmittedAttributes<kind::ResourceAttribute>, ApplyAdmissionErrors> {
    admit_apply_attributes::<kind::ResourceAttribute>(
        &resource.id,
        &resource.attributes,
        authored,
        &resource.quoted_string_attrs,
        schema,
        bindings,
        session,
        resolver,
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
    resolver: &dyn crate::parser::CustomTypeResolver,
) -> Result<AdmittedAttributes<kind::DataSourceAttribute>, ApplyAdmissionErrors> {
    admit_apply_attributes::<kind::DataSourceAttribute>(
        &resource.id,
        &resource.attributes,
        authored,
        &resource.quoted_string_attrs,
        schema,
        bindings,
        session,
        resolver,
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
    resolver: &dyn crate::parser::CustomTypeResolver,
) -> Result<AdmittedAttributes<K>, ApplyAdmissionErrors> {
    use std::cell::RefCell;
    let mut projected = std::collections::HashMap::new();
    let mut projection_errors = Vec::new();
    for (name, value) in resource_attributes {
        let value = authored
            .and_then(|(attributes, _)| attributes.get(name))
            .unwrap_or(value);
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
    let lookup = crate::parser::custom_type_lookup_for_schema(resolver, schema);
    let mut report = schema
        .schema()
        .as_schema()
        .validate_resolved_admission_report(
            &classified,
            authored.map(|(_, names)| names),
            &|name| quoted.contains(name),
            &lookup,
            &|name, _| {
                let destination = schema
                    .admission_attribute::<K>(id, name, session.catalog())
                    .expect("schema selected attribute");
                let result = Candidate::apply(projected[name].clone(), session).and_then(
                    |(candidate, mut boundary)| {
                        boundary.lookup = Some(&lookup);
                        admit(&destination, candidate, &boundary)
                    },
                );
                match result {
                    Ok(mut admission) => {
                        let findings = admission.take_findings();
                        match admission {
                            Admission::Ready(_placed) => {}
                            Admission::Pending(pending) => {
                                let error = AdmissionErrors::from(KnowledgeError::<
                                    AbsoluteBindingAddress,
                                >::UnresolvedAtBoundary {
                                    sites: pending.sites().to_vec(),
                                });
                                let findings =
                                    admission_findings::<K, NoPlaceholder, AbsoluteBindingAddress>(
                                        &mut Err(error.clone()),
                                    );
                                failures.borrow_mut().push(error);
                                return findings;
                            }
                        }
                        findings
                    }
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
        return Err(ApplyAdmissionErrors::Constraints(report));
    }
    Ok(AdmittedAttributes { kind: PhantomData })
}

/// The retained expression check used by the authored attribute walk.
type ValidationAssignmentCheck<'a, P, B> = Option<
    &'a dyn Fn(
        &str,
        &CompiledAttributeType,
        &Value<P, B>,
        &LeafPath,
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
    attributes: &std::collections::HashMap<
        String,
        crate::resource::knowledge::Classification<'_, P, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    bindings: Option<&BindingIndex<'_, B>>,
    session: &dyn ProviderTypeCheckContext,
    check: ValidationAssignmentCheck<'_, P, B>,
    declarations: Option<
        &std::collections::HashMap<String, (String, crate::binding_index::DeclaredValueSource)>,
    >,
) -> crate::schema::ResourceValueValidation {
    let lookup = crate::parser::custom_type_lookup_for_schema(
        session.catalog().custom_type_resolver(),
        schema,
    );
    schema.schema().as_schema().validate_admission_report(
        attributes,
        &|name| quoted.contains(name),
        &lookup,
        Some(&|name, proof| {
            let destination = schema
                .admission_attribute::<K>(id, name, session.catalog())
                .unwrap_or_else(|| schema.untyped_attribute(id, name, session.catalog()));
            let (candidate, mut boundary) =
                Candidate::validation_classified(proof.clone(), bindings, session);
            boundary.declaration = declarations
                .and_then(|sources| sources.get(name))
                .map(|(name, source)| (name.as_str(), source));
            let assignment =
                |attr: &CompiledAttributeType, value: &Value<P, B>, path: &LeafPath| match check {
                    Some(check) => check(name, attr, value, path),
                    None => Ok(Default::default()),
                };
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
                    assignment::reference_error(AdmissionReferenceError::Authored(Box::new(
                        finding,
                    )))
                })
                .collect()
            };
            boundary.assignment = Some(&assignment);
            boundary.untyped = Some(&untyped);
            let findings = admission_findings(&mut admit(&destination, candidate, &boundary));
            if proof.failure().is_some() {
                // Keep attempted failure evidence and attribute presence. The
                // expression/collector pass owns its validation diagnostic.
                findings.retained_failure()
            } else {
                findings
            }
        }),
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
    attributes: &std::collections::HashMap<
        String,
        crate::resource::knowledge::Classification<'_, crate::resource::ParserPlaceholder, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    report: &crate::parser::retained::ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
) -> crate::schema::ResourceValueValidation {
    if uses_resource_attribute_admission(resource.kind()) {
        typed_resource_attribute_admissions::<kind::ResourceAttribute, _, _, _>(
            resource, parsed, schema, attributes, quoted, report, bindings, session, schemas,
        )
    } else {
        typed_resource_attribute_admissions::<kind::DataSourceAttribute, _, _, _>(
            resource, parsed, schema, attributes, quoted, report, bindings, session, schemas,
        )
    }
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
    attributes: &std::collections::HashMap<
        String,
        crate::resource::knowledge::Classification<'_, crate::resource::ParserPlaceholder, B>,
    >,
    quoted: &std::collections::HashSet<String>,
    report: &crate::parser::retained::ExpressionReport<B>,
    bindings: &BindingIndex<'_, B>,
    session: &dyn ProviderTypeCheckContext,
    schemas: &SchemaRegistry,
) -> crate::schema::ResourceValueValidation {
    use crate::resource::{DeferredValue, ParserPlaceholder};
    let declarations = attributes
        .iter()
        .filter_map(|(name, proof)| {
            let expression = report
                .expression(
                    resource.attribute_value_location(name, &crate::schema::FieldPath::new()),
                )
                .ok()?;
            let value = expression.assignment_value(
                crate::resource::knowledge::ClassifiedRef::from(proof).diagnostic_value(),
            );
            crate::validation::module_resource_ref_declaration(
                parsed,
                resource.id(),
                name,
                value,
                bindings,
            )
            .map(|(source_name, source)| (name.clone(), (source_name.to_owned(), source)))
        })
        .collect();
    let check = |name: &str,
                 attr: &CompiledAttributeType,
                 value: &Value<ParserPlaceholder, B>,
                 _path: &LeafPath| {
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
        let relative =
            _path
                .segments()
                .iter()
                .skip(1)
                .fold(crate::schema::FieldPath::new(), |path, step| match step {
                    LeafPathSegment::StructField(name) => path.push_field(name),
                    LeafPathSegment::ListIndex(index) => path.push_index(*index),
                    LeafPathSegment::MapKey(key) => path.push_map_key(key.expose_plaintext()),
                });
        let expression = report
            .expression(resource.attribute_value_location(name, &relative))
            .or_else(|_| {
                report.expression(
                    resource.attribute_value_location(name, &crate::schema::FieldPath::new()),
                )
            })
            .map_err(|error| {
                assignment::reference_error(AdmissionReferenceError::Computed(
                    crate::validation::DeferredAccessTypeError::InvalidAccess {
                        error: error.into(),
                    },
                ))
            })?;
        let sink = attr
            .in_schema(schema.schema().as_schema().defs())
            .with_authority(
                &session.catalog().custom_types,
                crate::schema::SinkAuthority::Owner(schema.schema_type_owner()),
            );
        let judge_argument = |value| {
            let reference = crate::resource::module_argument_reference_path(value)?;
            let origin = format!("from {}", reference.as_ref().to_dot_string());
            let reference = ReferenceCandidate { reference };
            let contract = DestinationContract {
                attr: attr.clone(),
                defs: schema.schema().as_schema().defs().clone(),
                owner: schema.schema_type_owner().clone(),
                ancestry: FieldAncestry::WRITABLE,
                path: _path.clone(),
            };
            let (_, boundary) =
                Candidate::validation_classified(attributes[name].clone(), Some(bindings), session);
            let mut findings = Default::default();
            Some(
                reference
                    .judge_at::<K, ParserPlaceholder>(
                        &contract,
                        attr,
                        _path,
                        &boundary,
                        &mut findings,
                    )
                    .map(|()| findings)
                    .map_err(|error| match error {
                        TypeError::TypeMismatch { types } => {
                            assignment::reference_error(AdmissionReferenceError::Computed(
                                crate::validation::DeferredAccessTypeError::TypeMismatch {
                                    types,
                                    origin,
                                },
                            ))
                        }
                        error => error,
                    }),
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
                            match report.access(access) {
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
                let contract = DestinationContract {
                    attr: attr.clone(),
                    defs: schema.schema().as_schema().defs().clone(),
                    owner: schema.schema_type_owner().clone(),
                    ancestry: FieldAncestry::WRITABLE,
                    path: _path.clone(),
                };
                let (_, boundary) = Candidate::validation_classified(
                    attributes[name].clone(),
                    Some(bindings),
                    session,
                );
                let mut findings = Default::default();
                reference.judge_at::<K, ParserPlaceholder>(
                    &contract,
                    attr,
                    _path,
                    &boundary,
                    &mut findings,
                )?;
                return Ok(findings);
            }
        }
        match value {
            Value::Deferred(DeferredValue::Access(_) | DeferredValue::Coalesce { .. }) => {
                let expression = match value {
                    Value::Deferred(DeferredValue::Access(access)) => {
                        report.access(access).unwrap_or(expression)
                    }
                    Value::Concrete(_) | Value::Deferred(_) => expression,
                };
                crate::validation::check_deferred_access_type(
                    value, sink, expression, schemas, bindings,
                )
                .map_err(|error| {
                    assignment::reference_error(AdmissionReferenceError::Computed(error))
                })?;
            }
            Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
                if let Some(finding) = crate::validation::check_integer_arithmetic_types_with_sink(
                    value,
                    Some(crate::validation::RefSink::EffectiveType {
                        attr_type: attr,
                        defs: schema.schema().as_schema().defs(),
                        owner: schema.schema_type_owner(),
                        ancestry: FieldAncestry::WRITABLE,
                    }),
                    report,
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
    admit_validation_attributes::<K, _, _>(
        schema,
        resource.id(),
        attributes,
        quoted,
        Some(bindings),
        session,
        Some(&check),
        Some(&declarations),
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
                    catalog.as_provider_context().custom_type_resolver(),
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
