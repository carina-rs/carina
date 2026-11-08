//! Catalog-bound resource and data-source attribute descriptors.

use super::*;
use crate::resource::ResourceId;
use crate::schema::{LeafPathSegment, SchemaKind, SchemaRegistry};

fn owner(catalog: &ProviderContext, provider: &str) -> DeclarationOwner {
    let mut owners = std::collections::BTreeSet::new();
    owners.extend(
        catalog
            .resource_types
            .iter()
            .map(|(owner, _)| owner.as_str()),
    );
    owners.extend(
        catalog
            .known_custom_types()
            .iter()
            .flat_map(|identity| identity.provider_names()),
    );
    owners
        .iter()
        .position(|owner| *owner == provider)
        .map(DeclarationOwner::Catalog)
        .unwrap_or(DeclarationOwner::Builtin)
}

impl SchemaRegistry {
    fn admission_destination<K: DestinationKind>(
        &self,
        id: &ResourceId,
        kind: SchemaKind,
        catalog: &ProviderContext,
    ) -> Destination<K> {
        let schema = self
            .get(&id.provider, &id.resource_type, kind)
            .map(|schema| schema.schema().admission_identity)
            .unwrap_or(self.unavailable_admission_identity);
        Destination::catalogued(
            DestinationIdentity {
                declaration: DeclarationIdentity {
                    owner: owner(catalog, &id.provider),
                    schema,
                },
                instance: InstanceIdentity::Resource(id.clone()),
                path: vec![],
                revision: CatalogRevision(catalog.custom_types.clone()),
            },
            format!("{id}"),
        )
    }

    pub fn resource_destination(
        &self,
        id: &ResourceId,
        catalog: &ProviderContext,
    ) -> Destination<kind::ResourceAttribute> {
        self.admission_destination(id, SchemaKind::Resource, catalog)
    }

    pub fn data_source_destination(
        &self,
        id: &ResourceId,
        catalog: &ProviderContext,
    ) -> Destination<kind::DataSourceAttribute> {
        self.admission_destination(id, SchemaKind::DataSource, catalog)
    }

    fn admission_attribute<K: DestinationKind>(
        &self,
        id: &ResourceId,
        attribute: &str,
        kind: SchemaKind,
        catalog: &ProviderContext,
    ) -> Option<Destination<K>> {
        self.get(&id.provider, &id.resource_type, kind)?
            .admission_attribute(id, attribute, catalog)
    }

    pub fn resource_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Option<Destination<kind::ResourceAttribute>> {
        self.admission_attribute(id, attribute, SchemaKind::Resource, catalog)
    }

    pub fn data_source_attribute_destination(
        &self,
        id: &ResourceId,
        attribute: &str,
        catalog: &ProviderContext,
    ) -> Option<Destination<kind::DataSourceAttribute>> {
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
        Destination::catalogued(
            DestinationIdentity {
                declaration: DeclarationIdentity {
                    owner: owner(catalog, &id.provider),
                    schema: self.schema().admission_identity,
                },
                instance: InstanceIdentity::Resource(id.clone()),
                path: vec![],
                revision: CatalogRevision(catalog.custom_types.clone()),
            },
            name.into(),
        )
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
        let mut destination = Destination::catalogued(
            DestinationIdentity {
                declaration: DeclarationIdentity {
                    owner: owner(catalog, &id.provider),
                    schema: self.schema().admission_identity,
                },
                instance: InstanceIdentity::Resource(id.clone()),
                path: vec![],
                revision: CatalogRevision(catalog.custom_types.clone()),
            },
            attribute.into(),
        );
        destination.identity.path.push(ordinal);
        destination.label = attribute.into();
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

fn findings<K: DestinationKind, P: PlaceholderKind, B: BindingAddress>(
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
                findings(&mut admit(&destination, candidate, &boundary))
            },
        )
}

/// Every selected slot is filled only with a placement from this gate.
pub(crate) struct AdmittedAttributes<K: RechecksResolved> {
    _slots: std::collections::HashMap<String, PlacedValue<K>>,
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
    let projected = resource_attributes
        .iter()
        .map(|(name, value)| {
            let value = authored
                .and_then(|(attributes, _)| attributes.get(name))
                .unwrap_or(value);
            value
                .knowledge(&KnowledgeContext::apply(bindings))
                .map(|proof| (name.clone(), proof))
        })
        .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
    let classified = projected
        .iter()
        .map(|(name, proof)| (name.clone(), (**proof).clone()))
        .collect();
    let slots = RefCell::new(std::collections::HashMap::new());
    let failure = RefCell::new(None);
    let lookup = crate::parser::custom_type_lookup_for_schema(resolver, schema);
    let report = schema.schema().as_schema().validate_resolved_admission_report(
        &classified, authored.map(|(_, names)| names), &|name| quoted.contains(name), &lookup, &|name, _| {
            let destination = schema.admission_attribute::<K>(id, name, session.catalog()).expect("schema selected attribute");
            let result = Candidate::apply(projected[name].clone(), session).and_then(|(candidate, mut boundary)| {
                boundary.lookup = Some(&lookup);
                admit(&destination, candidate, &boundary)
            });
            match result {
                Ok(mut admission) => {
                    let findings = admission.take_findings();
                    match admission {
                        Admission::Ready(placed) => { slots.borrow_mut().insert(name.into(), placed); }
                        Admission::Pending(pending) => { *failure.borrow_mut() = Some(KnowledgeError::<AbsoluteBindingAddress>::UnresolvedAtBoundary { sites: pending.sites().to_vec() }.into()); }
                    }
                    findings
                }
                Err(mut error) => {
                    match error.take_findings() {
                        Some(findings) => findings,
                        None => { *failure.borrow_mut() = Some(error); Default::default() }
                    }
                }
            }
        },
    );
    if let Some(error) = failure.into_inner() {
        return Err(ApplyAdmissionErrors::Admission(error));
    }
    if report.errors().next().is_some() {
        return Err(ApplyAdmissionErrors::Constraints(report));
    }
    Ok(AdmittedAttributes {
        _slots: slots.into_inner(),
    })
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
            // The collector reports classification errors and retains Failed
            // inputs for presence checks. At validation, an admission failure
            // without schema/assignment findings must not manufacture another
            // diagnostic from that retained Knowledge evidence.
            match admit(&destination, candidate, &boundary) {
                Ok(mut admission) => admission.take_findings(),
                Err(mut error) => error.take_findings().unwrap_or_default(),
            }
        }),
    )
}

/// Retained expressions supply computed-access and arithmetic evidence to admit.
/// This adapter borrows the existing validator's descent; it has no value walker.
#[allow(clippy::too_many_arguments)]
pub fn resource_attribute_admissions<
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
            if let Some(finding) = errors.into_iter().next() {
                return Err(assignment::reference_error(
                    AdmissionReferenceError::Authored(Box::new(finding)),
                ));
            }
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
            if let Some(error) = computed.into_iter().next() {
                return Err(assignment::reference_error(
                    AdmissionReferenceError::Computed(error),
                ));
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
