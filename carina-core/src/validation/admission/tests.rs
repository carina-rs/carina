use super::*;
use crate::binding_index::BindingValueSource;
use crate::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
};
use crate::validation::deferred_types::with_session_for_test;
use std::collections::HashMap;

fn text(value: &str) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::String(value.into()))
}
fn reference() -> Value {
    Value::root_resource_ref("producer", "output", vec![])
}
fn string_type(name: &str, max: Option<u64>) -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::bare(name)),
        None,
        max.map(|max| (None, Some(max))),
        None,
    )
}

pub(super) fn fixture<R>(
    source: AttributeType,
    target: AttributeType,
    f: impl FnOnce(
        &ProviderContext,
        &SchemaRegistry,
        &BindingIndex<'_, AbsoluteBindingAddress>,
        &Destination<kind::ResourceAttribute>,
    ) -> R,
) -> R {
    let catalog = crate::parser::LoadedProviderContext::empty_for_test()
        .as_provider_context()
        .clone();
    fixture_in(&catalog, source, target, f)
}

fn fixture_in<R>(
    catalog: &ProviderContext,
    source: AttributeType,
    target: AttributeType,
    f: impl FnOnce(
        &ProviderContext,
        &SchemaRegistry,
        &BindingIndex<'_, AbsoluteBindingAddress>,
        &Destination<kind::ResourceAttribute>,
    ) -> R,
) -> R {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", source).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", target).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "let producer = test.Source { }\nlet sink = test.Sink { }",
        std::path::Path::new("admission.crn"),
        catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, catalog);
    let destination = schemas
        .resource_attribute_destination(&parsed.resources[1].id, "target", catalog)
        .unwrap();
    f(catalog, &schemas, &bindings, &destination)
}

fn validate<K: DestinationKind>(
    destination: &Destination<K>,
    value: &Value,
    bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
    session: &dyn ProviderTypeCheckContext,
) -> Result<Admission<K>, AdmissionErrors> {
    let (candidate, context) = Candidate::validation(
        value.knowledge(&KnowledgeContext::validation()).unwrap(),
        bindings,
        session,
    );
    admit(destination, candidate, &context)
}

#[test]
fn replacing_candidate_value_carries_all_original_obligations() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, bindings, _| {
            with_session_for_test(catalog, |session| {
                let value = reference();
                let (candidate, _) = Candidate::validation(
                    value.knowledge(&KnowledgeContext::validation()).unwrap(),
                    bindings,
                    session,
                );
                let original = candidate.clone();
                let resolved = text("fits");
                let bindings = ResolvedBindings::default();
                let (replaced, boundary) = candidate
                    .at_apply(
                        resolved
                            .knowledge(&KnowledgeContext::apply(&bindings))
                            .unwrap(),
                        session,
                    )
                    .unwrap();
                assert_eq!(replaced.obligations, original.obligations);
                assert_eq!(replaced.value, resolved);
                assert_eq!(replaced.obligations.identity, boundary.identity);
            });
        },
    );
}

#[test]
fn reference_without_bindings_is_never_proven_even_with_no_pending_sites() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, _, destination| {
            with_session_for_test(catalog, |session| {
                let value = text("fits");
                let bindings = ResolvedBindings::default();
                let proof = PlanResolved::try_from(
                    value.knowledge(&KnowledgeContext::plan(&bindings)).unwrap(),
                )
                .unwrap();
                let (_, context) = Candidate::plan(proof, session);
                assert!(context.sites().is_empty());
                let assignment::CandidateOrigin::Reference(reference) =
                    assignment::CandidateOrigin::of(&reference())
                else {
                    panic!("reference origin")
                };
                assert!(matches!(
                    reference.judge_assignment(destination, &context).unwrap(),
                    AssignmentVerdict::NeedsSchema(_)
                ));
            });
        },
    );
}

#[test]
fn wider_same_identity_length_records_destination_obligation() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let pending = validate(destination, &reference(), bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                assert_eq!(
                    &pending.local_obligation().unwrap().destination,
                    destination
                );
                assert_eq!(pending.sites().len(), 1);
            })
        },
    );
}

#[test]
fn resource_callers_admit_wider_same_identity_references() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, schemas, _, _| {
            let parsed = crate::parser::parse_file(
                "let producer = test.Source {}\nlet sink = test.Sink { target = producer.output }",
                std::path::Path::new("callers.crn"),
                catalog,
            )
            .unwrap()
            .resolve_root_for_test();
            let bindings = BindingIndex::from_parsed(&parsed, schemas, catalog);
            let configuration =
                crate::config_loader::load_buffer_configuration(&parsed, schemas, catalog);
            crate::validation::validate_resources(
                &parsed,
                schemas,
                &std::collections::HashSet::from(["test".into()]),
                catalog,
            )
            .unwrap();
            crate::validation::validate_resource_ref_types(
                configuration.judged(),
                &std::collections::HashSet::new(),
                &bindings,
            )
            .expect("same-identity resource reference is pending until resolution");
        },
    );
}

#[test]
fn declared_reference_defers_same_identity_local_constraints_at_resource_sink() {
    let source = string_type("GroupId", Some(64));
    fixture(
        source,
        string_type("GroupId", Some(36)),
        |catalog, schemas, bindings, destination| {
            let schema = schemas
                .get("test", "Source", crate::schema::SchemaKind::Resource)
                .unwrap();
            let source = schema
                .effective_attribute_by_authored_name("output")
                .unwrap();
            let declaration = crate::parser::TypeExpr::SchemaType {
                owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
                    "test",
                    source.field_type(),
                )
                .unwrap(),
            };
            with_session_for_test(catalog, |session| {
                let value = reference();
                let crate::binding_index::RefType::Typed(source) =
                    bindings.ref_type(&crate::resource::AccessPath::root("producer", "output"))
                else {
                    panic!("typed source");
                };
                let declared = source
                    .with_value_declaration(&declaration, bindings)
                    .unwrap();
                let (candidate, mut context) = Candidate::validation(
                    value.knowledge(&KnowledgeContext::validation()).unwrap(),
                    bindings,
                    session,
                );
                context.declaration = Some(("argument", &declared));
                let pending = admit(destination, candidate, &context)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                assert!(pending.local_obligation().is_some());
            });
        },
    );
}

#[test]
fn wider_same_identity_range_records_destination_obligation() {
    fixture(
        AttributeType::refined_int(
            Some(TypeIdentity::bare("Port")),
            Some((Some(0), Some(65535))),
        ),
        AttributeType::refined_int(
            Some(TypeIdentity::bare("Port")),
            Some((Some(1), Some(1024))),
        ),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                assert!(
                    validate(destination, &reference(), bindings, session)
                        .unwrap()
                        .ready()
                        .unwrap_err()
                        .local_obligation()
                        .is_some()
                );
            })
        },
    );
}

#[test]
fn different_identities_keep_strict_assignment() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("RoleArn", Some(36)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                assert!(matches!(
                    validate(destination, &reference(), bindings, session)
                        .unwrap_err()
                        .resolution_error(),
                    ResolutionError::AdmissionConstraints(_)
                ));
            })
        },
    );
}

#[test]
fn unmigrated_kind_keeps_strict_assignment() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let strict: Destination<kind::ModuleArgument> = Destination {
                    identity: destination.identity.clone(),
                    label: destination.label.clone(),
                    contract: destination.contract.clone(),
                    kind: PhantomData,
                };
                assert!(validate(&strict, &reference(), bindings, session).is_err());
            })
        },
    );
}

#[test]
fn violating_literal_is_rejected_at_validation() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(3)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let error =
                    validate(destination, &text("too long"), bindings, session).unwrap_err();
                let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                else {
                    panic!("typed constraints")
                };
                assert!(matches!(
                    findings.findings()[0].error,
                    TypeError::LengthOutOfRange { max: Some(3), .. }
                ));
            })
        },
    );
}

#[test]
fn discharge_checks_target_role_arn_and_preserves_error_and_path_once() {
    let source = string_type("Arn", None);
    let target = AttributeType::struct_(
        "Target",
        vec![
            StructField::required(
                "role_arn",
                AttributeType::refined_string(
                    Some(TypeIdentity::bare("Arn")),
                    None,
                    Some((Some(1), Some(1600))),
                    None,
                ),
            )
            .build(),
        ],
    );
    fixture(source, target, |catalog, _, bindings, destination| {
        with_session_for_test(catalog, |session| {
            let value = Value::Concrete(crate::resource::ConcreteValue::Map(
                indexmap::IndexMap::from([("role_arn".into(), reference())]),
            ));
            let pending = validate(destination, &value, bindings, session)
                .unwrap()
                .ready()
                .unwrap_err();
            assert!(pending.local_obligation().is_some());
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                "producer",
                HashMap::from([("output".into(), text(""))]),
                BindingValueSource::Local,
            );
            let error = pending.discharge(&resolved, session).unwrap_err();
            let ResolutionError::AdmissionConstraints(findings) = error.resolution_error() else {
                panic!("typed constraints")
            };
            let finding = &findings.findings()[0];
            assert_eq!(finding.path.to_string(), "target.role_arn");
            assert!(matches!(
                finding.error,
                TypeError::LengthOutOfRange {
                    min: Some(1),
                    max: Some(1600),
                    ..
                }
            ));
            assert_eq!(finding.error.attribute_name(), Some("target"));
            assert_eq!(findings.to_string().matches("target.role_arn").count(), 1);
        })
    });
}

#[test]
fn unresolved_discharge_is_an_admission_error() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let pending = validate(destination, &reference(), bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                let error = pending
                    .discharge(&ResolvedBindings::default(), session)
                    .unwrap_err();
                assert!(matches!(
                    error.resolution_error(),
                    ResolutionError::Knowledge(_)
                ));
            })
        },
    );
}

#[test]
fn plan_retains_apply_sites_and_discharge_places_fitting_value() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, _, destination| {
            with_session_for_test(catalog, |session| {
                let value = reference();
                let unresolved = ResolvedBindings::default();
                let proof = PlanResolved::try_from(
                    value
                        .knowledge(&KnowledgeContext::plan(&unresolved))
                        .unwrap(),
                )
                .unwrap();
                let sites = proof.knowledge().sites().collect::<Vec<_>>();
                let (candidate, context) = Candidate::plan(proof, session);
                let pending = admit(destination, candidate, &context)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                assert_eq!(pending.sites(), sites);
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("output".into(), text("fits"))]),
                    BindingValueSource::Local,
                );
                let placed = pending
                    .discharge(&resolved, session)
                    .unwrap()
                    .ready()
                    .unwrap();
                assert_eq!(placed.value(), &text("fits"));
                let mut slot = TypedSlot::new(destination.clone());
                slot.place(placed).unwrap();
                assert_eq!(slot.value(), Some(&text("fits")));
            })
        },
    );
}

#[test]
fn nested_list_map_struct_and_union_use_existing_value_checks() {
    let target = AttributeType::list(AttributeType::map(AttributeType::struct_(
        "Inner",
        vec![
            StructField::required(
                "value",
                AttributeType::union(vec![
                    AttributeType::refined_string(None, None, Some((Some(1), Some(3))), None),
                    AttributeType::int(),
                ]),
            )
            .build(),
        ],
    )));
    fixture(
        AttributeType::string(),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let inner = Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([("value".into(), text("too long"))]),
                ));
                let map = Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([("key.with.dots".into(), inner)]),
                ));
                let value = Value::Concrete(crate::resource::ConcreteValue::List(vec![map]));
                let error = validate(destination, &value, bindings, session).unwrap_err();
                let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                else {
                    panic!("typed constraints")
                };
                assert_eq!(
                    findings.findings()[0].path.to_string(),
                    "target[0][\"key.with.dots\"].value"
                );
            })
        },
    );
}

#[test]
fn secret_errors_remain_protected() {
    fixture(
        AttributeType::string(),
        AttributeType::refined_string(None, None, Some((None, Some(3))), None),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Deferred(crate::resource::DeferredValue::Secret(
                    Box::new(text("admission-secret-plaintext")).into(),
                ));
                let error = validate(destination, &value, bindings, session).unwrap_err();
                assert!(!format!("{error:?}").contains("admission-secret-plaintext"));
                assert!(
                    !error
                        .resolution_error()
                        .to_string()
                        .contains("admission-secret-plaintext")
                );
            })
        },
    );
}

#[test]
fn slot_identity_mismatches_do_not_mutate() {
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let placed = validate(destination, &text("original"), bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap();
                let mut slot = TypedSlot::new(destination.clone());
                slot.place(placed.clone()).unwrap();
                for mismatch in [
                    SlotMismatch::Declaration,
                    SlotMismatch::Instance,
                    SlotMismatch::Path,
                    SlotMismatch::CatalogRevision,
                ] {
                    let mut wrong = placed.clone();
                    match mismatch {
                        SlotMismatch::Declaration => {
                            wrong.inner.destination.identity.declaration.schema =
                                DeclarationId::default()
                        }
                        SlotMismatch::Instance => {
                            wrong.inner.destination.identity.instance = InstanceIdentity::Resource(
                                crate::resource::ResourceId::with_provider_identity(
                                    "test", "Sink", "other", None,
                                ),
                            )
                        }
                        SlotMismatch::Path => wrong.inner.destination.identity.path.push(99),
                        SlotMismatch::CatalogRevision => {
                            wrong.inner.destination.identity.revision = CatalogRevision(
                                ProviderContext::new(crate::EnvironmentSnapshot::empty())
                                    .custom_types,
                            )
                        }
                    }
                    assert_eq!(slot.place(wrong), Err(mismatch));
                    assert_eq!(slot.value(), Some(&text("original")));
                }
            })
        },
    );
}

fn infra_case(binding: &str, nested: bool, uuid: bool) {
    let identity = if uuid { "GroupId" } else { "Arn" };
    let source = string_type(identity, uuid.then_some(64));
    let scalar = AttributeType::refined_string(
        Some(TypeIdentity::bare(identity)),
        uuid.then(|| {
            crate::schema::PatternSpec::new(
                "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
                crate::schema::PatternMatch::Full,
            )
        }),
        Some((Some(1), Some(if uuid { 36 } else { 1600 }))),
        None,
    );
    let target = if nested {
        AttributeType::struct_(
            "Target",
            vec![StructField::required("role_arn", scalar).build()],
        )
    } else {
        scalar
    };
    let catalog = crate::parser::LoadedProviderContext::empty_for_test()
        .as_provider_context()
        .clone();
    let mut schemas = SchemaRegistry::new();
    let (source_type, source_name, source_attribute, sink_type, attribute) = if uuid {
        (
            "identitystore.Group",
            "admins",
            "group_id",
            "sso.Assignment",
            "principal_id",
        )
    } else {
        (
            "iam.Role",
            "scheduler_role",
            "arn",
            "scheduler.Schedule",
            "target",
        )
    };
    schemas.insert(
        "aws",
        ResourceSchema::new(source_type)
            .attribute(AttributeSchema::optional(source_attribute, source).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new(sink_type)
            .attribute(AttributeSchema::optional(attribute, target).build())
            .validate()
            .unwrap(),
    );
    let input = if uuid && binding.contains("loop") {
        "let admins = aws.identitystore.Group {}\nfor account in ['111111111111', '222222222222'] { awscc.sso.Assignment { principal_id = admins.group_id } }".to_owned()
    } else if uuid {
        "let admins = aws.identitystore.Group {}\nlet assignment = awscc.sso.Assignment { principal_id = admins.group_id }".to_owned()
    } else {
        "let scheduler_role = aws.iam.Role {}\nlet first = awscc.scheduler.Schedule { target = { role_arn = scheduler_role.arn } }\nlet second = awscc.scheduler.Schedule { target = { role_arn = scheduler_role.arn } }".to_owned()
    };
    let parsed = crate::parser::parse_file(
        &input,
        std::path::Path::new("infra-regression.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, &catalog);
    let index = if binding.contains("second") || binding.contains("loop") {
        2
    } else {
        1
    };
    let destination = schemas
        .resource_attribute_destination(&parsed.resources[index].id, attribute, &catalog)
        .unwrap();
    let bindings = &bindings;
    let destination = &destination;
    with_session_for_test(&catalog, |session| {
        let wrap = |value| {
            if nested {
                Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([("role_arn".into(), value)]),
                ))
            } else {
                value
            }
        };
        let symbolic = wrap(Value::root_resource_ref(
            source_name,
            source_attribute,
            vec![],
        ));
        assert!(
            validate(destination, &symbolic, bindings, session)
                .unwrap()
                .ready()
                .is_err(),
            "{binding}"
        );
        for (output, fits) in [
            (
                if uuid {
                    "12345678-1234-1234-1234-123456789abc".to_string()
                } else {
                    "arn:aws:iam::123456789012:role/scheduler".to_string()
                },
                true,
            ),
            (
                if uuid {
                    "a".repeat(64)
                } else {
                    "x".repeat(1601)
                },
                false,
            ),
            (
                if uuid {
                    "invalid-uuid".to_string()
                } else {
                    String::new()
                },
                false,
            ),
        ] {
            let literal = validate(destination, &wrap(text(&output)), bindings, session);
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                source_name,
                HashMap::from([(source_attribute.into(), text(&output))]),
                BindingValueSource::Local,
            );
            let projection =
                crate::resolver::resolve_ref_value(&symbolic, &resolved, session).unwrap();
            let proof = PlanResolved::try_from(
                projection
                    .knowledge(&KnowledgeContext::plan(&resolved))
                    .unwrap(),
            )
            .unwrap();
            let (candidate, context) = Candidate::plan(proof, session);
            let plan = admit(destination, candidate, &context);
            let pending = validate(destination, &symbolic, bindings, session)
                .unwrap()
                .ready()
                .unwrap_err();
            let apply = pending.discharge(&resolved, session);
            if fits {
                assert_eq!(
                    plan.unwrap().ready().unwrap().value(),
                    &wrap(text(&output)),
                    "{binding}"
                );
                assert_eq!(
                    apply.unwrap().ready().unwrap().value(),
                    &wrap(text(&output)),
                    "{binding}"
                );
                literal.unwrap().ready().unwrap();
            } else {
                let literal = literal.unwrap_err();
                assert_eq!(
                    plan.unwrap_err().resolution_error(),
                    literal.resolution_error(),
                    "{binding}"
                );
                assert_eq!(
                    apply.unwrap_err().resolution_error(),
                    literal.resolution_error(),
                    "{binding}"
                );
            }
        }
    });
}

#[test]
fn infra_identitystore_group_id_to_assignment_principal_id_direct() {
    infra_case("admins.group_id direct", false, true);
}
#[test]
fn infra_identitystore_group_id_to_assignment_principal_id_in_loop() {
    infra_case("admins.group_id loop instance", false, true);
}
#[test]
fn infra_scheduler_role_arn_to_schedule_target_role_arn_first() {
    infra_case("publish schedule first", true, false);
}
#[test]
fn infra_scheduler_role_arn_to_schedule_target_role_arn_second() {
    infra_case("publish schedule second", true, false);
}

#[test]
fn native_provider_callbacks_run_once_and_only_after_local_checks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let target = AttributeType::list(AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Short"),
        None,
        Some((None, Some(3))),
        crate::schema::validator(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        None,
    ));
    fixture(
        AttributeType::string(),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Concrete(crate::resource::ConcreteValue::List(vec![
                    text("too long"),
                    text("ok"),
                ]));
                assert!(validate(destination, &value, bindings, session).is_err());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            })
        },
    );
}

#[test]
fn candidate_cannot_use_another_candidates_boundary_proof() {
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let first = text("first");
                let second = text("second");
                let (candidate, _) = Candidate::validation(
                    first.knowledge(&KnowledgeContext::validation()).unwrap(),
                    bindings,
                    session,
                );
                let (_, context) = Candidate::validation(
                    second.knowledge(&KnowledgeContext::validation()).unwrap(),
                    bindings,
                    session,
                );
                assert!(admit(destination, candidate, &context).is_err());
            })
        },
    );
}

#[test]
fn pending_is_holdable_and_discharged_for_every_unmigrated_kind() {
    fn check<K: DestinationKind + std::fmt::Debug>(
        destination: &Destination<kind::ResourceAttribute>,
        bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
        session: &dyn ProviderTypeCheckContext,
    ) {
        let destination: Destination<K> = Destination {
            identity: destination.identity.clone(),
            label: destination.label.clone(),
            contract: destination.contract.clone(),
            kind: PhantomData,
        };
        let pending: PendingAdmission<K> = validate(&destination, &reference(), bindings, session)
            .unwrap()
            .ready()
            .unwrap_err();
        assert!(pending.local_obligation().is_none());
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("ok"))]),
            BindingValueSource::Local,
        );
        assert_eq!(
            pending
                .discharge(&resolved, session)
                .unwrap()
                .ready()
                .unwrap()
                .value(),
            &text("ok")
        );
    }
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                check::<kind::ProviderConfigAttribute>(destination, bindings, session);
                check::<kind::BackendAttribute>(destination, bindings, session);
                check::<kind::ModuleArgument>(destination, bindings, session);
                check::<kind::AnnotatedAttribute>(destination, bindings, session);
                check::<kind::Export>(destination, bindings, session);
                check::<kind::UpstreamExport>(destination, bindings, session);
            })
        },
    );
}

#[test]
fn missing_schema_never_becomes_placement_at_apply() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let registry = SchemaRegistry::new();
    let id = crate::resource::ResourceId::with_provider_identity("test", "Sink", "sink", None);
    let destination = registry.resource_destination(&id, &catalog);
    with_session_for_test(&catalog, |session| {
        let value = text("known");
        let proof = PlanResolved::try_from(
            value
                .knowledge(&KnowledgeContext::plan(&ResolvedBindings::default()))
                .unwrap(),
        )
        .unwrap();
        let (candidate, context) = Candidate::plan(proof, session);
        let pending = admit(&destination, candidate, &context)
            .unwrap()
            .ready()
            .unwrap_err();
        assert!(matches!(
            pending
                .discharge(&ResolvedBindings::default(), session)
                .unwrap_err()
                .resolution_error(),
            ResolutionError::Knowledge(_)
        ));
    });
}

#[test]
fn different_identity_edges_do_not_relax_local_containment() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("RoleArn", Some(36)),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = reference();
                let Value::Deferred(crate::resource::DeferredValue::ResourceRef { path }) = &value
                else {
                    panic!("reference")
                };
                let crate::binding_index::RefType::Typed(source) = bindings.ref_type(path) else {
                    panic!("source")
                };
                let contract = destination.contract.as_deref().unwrap();
                assert!(matches!(
                    source.constraints().next().unwrap().assignment_to(
                        contract.attr.in_schema(&contract.defs).with_authority(
                            &catalog.custom_types,
                            crate::schema::SinkAuthority::Owner(&contract.owner)
                        )
                    ),
                    crate::schema::TypeAssignability::Mismatch
                ));
                assert!(validate(destination, &reference(), bindings, session).is_err());
            })
        },
    );
}

#[test]
fn nested_union_references_retain_local_paths() {
    let target = AttributeType::union(vec![
        AttributeType::struct_(
            "Target",
            vec![StructField::required("role_arn", string_type("Arn", Some(3))).build()],
        ),
        AttributeType::int(),
    ]);
    fixture(
        string_type("Arn", None),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([("role_arn".into(), reference())]),
                ));
                let pending = validate(destination, &value, bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                assert_eq!(
                    pending.local_obligation().unwrap().paths()[0].to_string(),
                    "target.role_arn"
                );
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("output".into(), text("long"))]),
                    BindingValueSource::Local,
                );
                assert!(pending.discharge(&resolved, session).is_err());
            })
        },
    );
}

#[test]
fn active_failure_is_an_error_but_unselected_fallback_failure_is_lazy() {
    let failed = Value::Deferred(crate::resource::DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![],
        reason: crate::resource::UnavailableReason::from_builtin_error("already attempted".into()),
        source_line: 0.into(),
    });
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                assert!(matches!(
                    validate(destination, &failed, bindings, session)
                        .unwrap_err()
                        .resolution_error(),
                    ResolutionError::Knowledge(_)
                ));
                let fallback = Value::Deferred(crate::resource::DeferredValue::Coalesce {
                    value: Box::new(reference()),
                    default: Box::new(failed),
                });
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("output".into(), text("selected"))]),
                    BindingValueSource::Local,
                );
                let projection =
                    crate::resolver::resolve_ref_value(&fallback, &resolved, session).unwrap();
                let proof = PlanResolved::try_from(
                    projection
                        .knowledge(&KnowledgeContext::plan(&resolved))
                        .unwrap(),
                )
                .unwrap();
                let (candidate, context) = Candidate::plan(proof, session);
                assert_eq!(
                    admit(destination, candidate, &context)
                        .unwrap()
                        .ready()
                        .unwrap()
                        .value(),
                    &text("selected")
                );
            })
        },
    );
}

#[test]
fn protected_pending_local_paths_never_expose_map_keys() {
    let target = AttributeType::map(string_type("Arn", Some(3)));
    fixture(
        string_type("Arn", None),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Deferred(crate::resource::DeferredValue::Secret(
                    Box::new(Value::Concrete(crate::resource::ConcreteValue::Map(
                        indexmap::IndexMap::from([("private-admission-key".into(), reference())]),
                    )))
                    .into(),
                ));
                let pending = validate(destination, &value, bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                assert!(!format!("{pending:?}").contains("private-admission-key"));
                assert!(
                    !pending.local_obligation().unwrap().paths()[0]
                        .to_string()
                        .contains("private-admission-key")
                );
            })
        },
    );
}

#[test]
fn unavailable_reference_authority_cannot_be_erased_by_resolution() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", string_type("SourceId", None)).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::refined_string_with_validator(
                        TypeIdentity::bare("TargetId"),
                        None,
                        None,
                        crate::schema::validator(|_| Ok(())),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "let producer = test.Source {}\nlet sink = test.Sink {}",
        std::path::Path::new("missing-authority.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(&parsed.resources[1].id, "target", &catalog)
        .unwrap();
    with_session_for_test(&catalog, |session| {
        let pending = validate(&destination, &reference(), &bindings, session)
            .unwrap()
            .ready()
            .unwrap_err();
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("resolved"))]),
            BindingValueSource::Local,
        );
        assert!(pending.discharge(&resolved, session).is_err());
    });
}

#[test]
fn data_source_attributes_use_the_same_local_obligation_and_discharge() {
    let catalog = crate::parser::LoadedProviderContext::empty_for_test()
        .as_provider_context()
        .clone();
    let mut schemas = SchemaRegistry::new();
    for (name, max) in [("Source", 64), ("Sink", 36)] {
        schemas.insert(
            "test",
            ResourceSchema::new(name)
                .as_data_source()
                .attribute(
                    AttributeSchema::optional(
                        if name == "Source" { "output" } else { "target" },
                        string_type("GroupId", Some(max)),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
    }
    let parsed = crate::parser::parse_file(
        "let producer = read test.Source {}\nlet sink = read test.Sink {}",
        std::path::Path::new("data-source-admission.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, &catalog);
    let destination = schemas
        .data_source_attribute_destination(&parsed.data_sources[1].id, "target", &catalog)
        .unwrap();
    with_session_for_test(&catalog, |session| {
        let pending = validate(&destination, &reference(), &bindings, session)
            .unwrap()
            .ready()
            .unwrap_err();
        assert_eq!(
            pending.local_obligation().unwrap().paths()[0].to_string(),
            "target"
        );
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("fits"))]),
            BindingValueSource::Local,
        );
        assert_eq!(
            pending
                .clone()
                .discharge(&resolved, session)
                .unwrap()
                .ready()
                .unwrap()
                .value(),
            &text("fits")
        );
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text(&"x".repeat(64)))]),
            BindingValueSource::Local,
        );
        assert!(pending.discharge(&resolved, session).is_err());
    });
}

#[test]
fn provider_callbacks_preserve_union_selection_and_string_list_policy() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
        std::collections::HashSet::new(),
        Box::new(move |target, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            let crate::parser::CustomTypeValidationTarget::Annotation(identity) = target else {
                panic!("fixture annotation")
            };
            assert_eq!(identity.provider_names().collect::<Vec<_>>(), vec!["test"]);
            if identity.identity().kind == "First" {
                Err(crate::parser::CustomTypeValidationError::Rejected {
                    owner: "test".into(),
                    identity: identity.identity().clone(),
                    message: "first arm rejected".into(),
                })
            } else {
                Ok(())
            }
        }),
    );
    let target = AttributeType::list(AttributeType::union(vec![
        string_type("First", Some(3)),
        string_type("Second", Some(10)),
    ]));
    fixture_in(
        &catalog,
        AttributeType::string(),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let list = Value::Concrete(crate::resource::ConcreteValue::List(vec![
                    text("abcd"),
                    text("ok"),
                ]));
                validate(destination, &list, bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 3);
                calls.store(0, Ordering::SeqCst);
                let compact = Value::Concrete(crate::resource::ConcreteValue::StringList(vec![
                    "abcd".into(),
                    "ok".into(),
                ]));
                validate(destination, &compact, bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            })
        },
    );
}

#[test]
fn reference_findings_retain_the_original_lookup_failure() {
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::root_resource_ref("missing-binding", "output", vec![]);
                let error = validate(destination, &value, bindings, session).unwrap_err();
                assert!(
                    error
                        .resolution_error()
                        .to_string()
                        .contains("missing-binding")
                );
                let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                else {
                    panic!("typed finding")
                };
                assert!(
                    matches!(&findings.findings()[0].error, TypeError::Reference { error, .. }
            if matches!(error.as_ref(), crate::resource::SecretDiagnosticError::Public(AdmissionReferenceError::UnknownBinding { binding, .. }) if binding == "missing-binding"))
                );
            })
        },
    );
}

#[test]
fn active_failure_is_reported_even_when_destination_schema_is_missing() {
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let mut missing = destination.clone();
                missing.contract = None;
                let failed = Value::Deferred(crate::resource::DeferredValue::Unavailable {
                    name: crate::builtins::BuiltinFunction::env,
                    args: vec![],
                    reason: crate::resource::UnavailableReason::from_builtin_error(
                        "attempted".into(),
                    ),
                    source_line: 0.into(),
                });
                assert!(validate(&missing, &failed, bindings, session).is_err());
            })
        },
    );
}

#[test]
fn conversion_error_display_label_remains_available_for_4260() {
    let source = ResolutionError::Message("original conversion failure".into());
    let error = AdmissionErrors::conversion(source.clone());
    assert_eq!(error.to_string(), "conversion admission failed");
    assert_eq!(error.resolution_error(), &source);
    assert_eq!(error.into_resolution_error(), source);
    assert_eq!(
        AdmissionErrors::from(source).to_string(),
        "value admission failed"
    );
}

#[test]
fn container_and_child_findings_keep_their_own_canonical_locations() {
    let target = AttributeType::struct_(
        "Target",
        vec![
            StructField::required("missing", AttributeType::string()).build(),
            StructField::required("value", string_type("Short", Some(3))).build(),
        ],
    );
    fixture(
        AttributeType::string(),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([("value".into(), text("too long"))]),
                ));
                let error = validate(destination, &value, bindings, session).unwrap_err();
                let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                else {
                    panic!("located constraints")
                };
                assert_eq!(
                    findings
                        .findings()
                        .iter()
                        .map(|finding| finding.path.to_string())
                        .collect::<Vec<_>>(),
                    vec!["target.missing", "target.value"]
                );
                assert!(matches!(
                    findings.findings()[0].error,
                    TypeError::MissingRequired { .. }
                ));
                assert!(matches!(
                    findings.findings()[1].error,
                    TypeError::LengthOutOfRange { .. }
                ));
            })
        },
    );
}

#[test]
fn nested_field_group_renders_the_destination_path_once() {
    let choices = AttributeType::struct_with_field_groups(
        "Choices",
        vec![
            StructField::optional("first", AttributeType::string()).build(),
            StructField::optional("second", AttributeType::string()).build(),
        ],
        vec![crate::schema::FieldGroup::at_least_one(["first", "second"]).unwrap()],
    );
    let target = AttributeType::struct_(
        "Target",
        vec![StructField::required("choices", choices).build()],
    );
    fixture(
        AttributeType::string(),
        target,
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let value = Value::Concrete(crate::resource::ConcreteValue::Map(
                    indexmap::IndexMap::from([(
                        "choices".into(),
                        Value::Concrete(crate::resource::ConcreteValue::Map(
                            indexmap::IndexMap::new(),
                        )),
                    )]),
                ));
                let error = validate(destination, &value, bindings, session).unwrap_err();
                assert_eq!(
                    error.resolution_error().to_string(),
                    "target.choices: at least one of [first, second] must be set"
                );
            })
        },
    );
}
