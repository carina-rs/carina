use super::*;
use crate::binding_index::BindingValueSource;
use crate::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
};
use crate::validation::deferred_types::with_session_for_test;
use std::collections::HashMap;

#[path = "tests/seam_round4.rs"]
mod seam_round4;

#[path = "tests/substituted_reference.rs"]
mod substituted_reference;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AdmissionWork {
    candidates: usize,
    contracts: usize,
    references: usize,
}
thread_local! {
    static ADMISSION_WORK: std::cell::Cell<Option<AdmissionWork>> = const { std::cell::Cell::new(None) };
}
fn record_work(record: impl FnOnce(&mut AdmissionWork)) {
    ADMISSION_WORK.with(|work| {
        if let Some(mut counts) = work.get() {
            record(&mut counts);
            work.set(Some(counts));
        }
    });
}
pub(super) fn candidate_constructed() {
    record_work(|counts| counts.candidates += 1);
}
pub(super) fn contract_constructed() {
    record_work(|counts| counts.contracts += 1);
}
pub(super) fn reference_judged() {
    record_work(|counts| counts.references += 1);
}
fn count_admission_work<R>(operation: impl FnOnce() -> R) -> (R, AdmissionWork) {
    struct Reset(Option<AdmissionWork>);
    impl Drop for Reset {
        fn drop(&mut self) {
            ADMISSION_WORK.set(self.0);
        }
    }
    let _reset = Reset(ADMISSION_WORK.replace(Some(AdmissionWork::default())));
    let result = operation();
    (result, ADMISSION_WORK.get().unwrap())
}

fn text(value: &str) -> Value {
    Value::Concrete(crate::resource::ConcreteValue::String(value.into()))
}
fn reference() -> Value {
    Value::root_resource_ref("producer", "output", vec![])
}
fn string_slot(
    name: &str,
    pattern: Option<crate::schema::PatternSpec>,
    length: Option<(Option<u64>, Option<u64>)>,
) -> AttributeType {
    AttributeType::string_member(
        crate::schema::StringIdentityLayer::new(Some(TypeIdentity::bare(name)), None, None, None),
        crate::schema::StringMemberConstraints::new(pattern, length),
    )
}
fn string_type(name: &str, max: Option<u64>) -> AttributeType {
    string_slot(name, None, max.map(|max| (None, Some(max))))
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

fn validate<K: AssignmentKind>(
    destination: &Destination<K>,
    value: &Value,
    bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
    session: &dyn ProviderTypeCheckContext,
) -> Result<Admission<K>, AdmissionErrors> {
    let (candidate, context) = Candidate::validation_classified(
        value
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .into_classification(),
        Some(bindings),
        session,
    );
    admit(destination, candidate, &context)
}

#[test]
fn destination_contracts_share_the_registered_schema_allocation() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("Sink")
            .with_def("Entry", AttributeType::string())
            .attribute(AttributeSchema::optional("first", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("second", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let id = crate::resource::ResourceId::with_provider_identity("test", "Sink", "sink", None);
    let registered = schemas
        .get("test", "Sink", crate::schema::SchemaKind::Resource)
        .unwrap();
    for name in ["first", "second"] {
        let destination = schemas
            .resource_attribute_destination(&id, name, &catalog)
            .unwrap();
        for destination in [&destination, &destination.clone()] {
            let contract = destination.contract.as_deref().unwrap();
            let schema = &contract.schema;
            assert!(std::ptr::eq(
                schema.as_ref(),
                registered.schema().as_schema()
            ));
            assert!(std::ptr::eq(
                contract.defs(),
                registered.schema().as_schema().defs()
            ));
            assert!(std::ptr::eq(
                contract.attr(),
                registered
                    .effective_attribute_by_authored_name(name)
                    .unwrap()
                    .field_type(),
            ));
        }
    }
}

#[test]
fn retained_reference_boundaries_are_built_per_attribute_not_per_leaf() {
    let catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional("first", AttributeType::list(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("second", AttributeType::list(AttributeType::string()))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    for leaf in ["input", "\"${input}\"", "\"${producer.output}\""] {
        for count in [1, 64] {
            let items = vec![leaf; count].join(", ");
            let parsed = crate::parser::parse_file(
                &format!("arguments {{ input: String }}\nlet producer = test.Source {{}}\nlet sink = test.Sink {{ first = [{items}] second = [{items}] }}"),
                std::path::Path::new("admission-work.crn"),
                &catalog,
            ).unwrap();
            let configuration =
                crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
            let judged = configuration.judged();
            let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
            let resource = tree
                .resources()
                .find(|resource| resource.id().resource_type == "Sink")
                .unwrap();
            let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
            let inputs = attributes::ResourceAttributeInputs::new(resource);
            let attributes = inputs.classify(&KnowledgeContext::validation(), |_, error| {
                panic!("classification: {error:?}")
            });
            with_session_for_test(&catalog, |session| {
                let (findings, work) = count_admission_work(|| {
                    attributes::resource_attribute_admissions(
                        resource,
                        judged.file(),
                        resource.schema(&schemas).unwrap(),
                        &attributes,
                        resource.quoted_string_attrs(),
                        &inputs,
                        &bindings,
                        session,
                        &schemas,
                        &mut Default::default(),
                    )
                });
                assert_eq!(findings.errors().count(), 0, "{leaf}: {findings:?}");
                assert_eq!(
                    findings.symbolic_errors().count(),
                    0,
                    "{leaf}: {findings:?}"
                );
                assert_eq!(
                    work,
                    AdmissionWork {
                        candidates: 2,
                        contracts: 2,
                        references: 2 * count
                    },
                    "{leaf}, {count} leaves"
                );
            });
        }
    }
}

#[test]
fn ambiguous_union_rejects_reference_fitting_no_member_and_checks_existence() {
    fixture(
        AttributeType::int(),
        AttributeType::union(vec![
            AttributeType::map(AttributeType::string()),
            AttributeType::struct_(
                "Count",
                vec![StructField::optional("count", AttributeType::string()).build()],
            ),
        ]),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let map = |value| {
                    Value::Concrete(crate::resource::ConcreteValue::Map(
                        indexmap::IndexMap::from([("count".into(), value)]),
                    ))
                };
                let error =
                    validate(destination, &map(reference()), bindings, session).unwrap_err();
                let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                else {
                    panic!("expected a validation rejection: {error:?}");
                };
                assert_eq!(findings.findings().len(), 1, "{error:?}");
                assert!(
                    findings.findings()[0]
                        .error
                        .to_string()
                        .contains("expected String, got Int"),
                    "{error:?}"
                );
                assert!(
                    validate(
                        destination,
                        &map(Value::root_resource_ref("missing", "output", vec![])),
                        bindings,
                        session,
                    )
                    .is_err()
                );
            });
        },
    );
}

#[test]
fn ambiguous_union_checks_no_member_and_all_members_for_every_kind() {
    ambiguous_union_checks_members::<kind::ResourceAttribute>();
    ambiguous_union_checks_members::<kind::DataSourceAttribute>();
    ambiguous_union_checks_members::<kind::ModuleArgument>();
    ambiguous_union_checks_members::<kind::AnnotatedAttribute>();
    ambiguous_union_checks_members::<kind::Export>();
    ambiguous_union_checks_members::<kind::UpstreamExport>();
}

fn ambiguous_union_checks_members<K: AssignmentKind + std::fmt::Debug>() {
    for (source, accepted) in [
        (AttributeType::int(), false),
        (AttributeType::string(), true),
    ] {
        for reverse in [false, true] {
            let mut members = vec![
                AttributeType::map(AttributeType::string()),
                AttributeType::struct_(
                    "Count",
                    vec![StructField::optional("count", AttributeType::string()).build()],
                ),
            ];
            if reverse {
                members.reverse();
            }
            fixture(
                source.clone(),
                AttributeType::union(members),
                |catalog, _, bindings, destination| {
                    let destination: Destination<K> = Destination {
                        identity: destination.identity.clone(),
                        label: destination.label.clone(),
                        contract: destination.contract.clone(),
                        kind: PhantomData,
                    };
                    with_session_for_test(catalog, |session| {
                        let value = Value::Concrete(crate::resource::ConcreteValue::Map(
                            indexmap::IndexMap::from([("count".into(), reference())]),
                        ));
                        let (candidate, context) = Candidate::validation_classified(
                            value
                                .knowledge(&KnowledgeContext::validation())
                                .unwrap()
                                .into_classification(),
                            Some(bindings),
                            session,
                        );
                        let judgement =
                            assignment::judge_with_findings(&destination, &candidate, &context);
                        if accepted {
                            assert!(matches!(
                                judgement.unwrap().verdict,
                                AssignmentVerdict::Proven(_)
                            ));
                            let pending = admit(&destination, candidate, &context)
                                .unwrap()
                                .ready()
                                .unwrap_err();
                            assert!(pending.local_obligation().is_none());
                        } else {
                            assert!(judgement.is_err(), "Int fits no candidate member");
                            assert!(admit(&destination, candidate, &context).is_err());
                        }
                    });
                },
            );
        }
    }
}

#[test]
fn replacing_candidate_value_carries_all_original_obligations() {
    fixture(
        string_type("GroupId", Some(64)),
        string_type("GroupId", Some(36)),
        |catalog, _, bindings, _| {
            with_session_for_test(catalog, |session| {
                let value = reference();
                let (candidate, _) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(bindings),
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
fn needs_local_without_pending_sites_remains_pending() {
    fixture(
        AttributeType::string(),
        string_type("GroupId", Some(36)),
        |catalog, _, _, destination| {
            with_session_for_test(catalog, |session| {
                let value = text("fits");
                let (candidate, mut context) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    None,
                    session,
                );
                let local_check = |_: &CompiledAttributeType,
                                   _: &crate::resource::Value<
                    crate::resource::NoPlaceholder,
                    AbsoluteBindingAddress,
                >,
                                   path: &LeafPath,
                                   _: crate::parser::retained::AuthoredCursor<
                    '_,
                    AbsoluteBindingAddress,
                >| {
                    Ok(crate::schema::value_validation::AssignmentFindings {
                        local: vec![assignment::LocalPath::for_recheck(
                            kind::ResourceAttribute,
                            path.clone(),
                        )],
                        ..Default::default()
                    })
                };
                context.assignment.ordinary = Some(&local_check);
                assert!(context.sites().is_empty());
                assert!(matches!(
                    assignment::judge_with_findings(destination, &candidate, &context)
                        .unwrap()
                        .verdict,
                    AssignmentVerdict::NeedsLocal(_)
                ));
                let pending = admit(destination, candidate, &context)
                    .unwrap()
                    .ready()
                    .expect_err("NeedsLocal must retain its obligation even without pending sites");
                assert!(pending.sites().is_empty());
                assert!(pending.local_obligation().is_some());
                assert_eq!(
                    pending
                        .discharge(&ResolvedBindings::default(), session)
                        .unwrap()
                        .value(),
                    &value
                );
            });
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
                &mut Vec::new(),
            )
            .unwrap();
            crate::validation::validate_resource_ref_types(
                configuration.judged(),
                &std::collections::HashSet::new(),
                &bindings,
                &mut Vec::new(),
            )
            .expect("same-identity resource reference is pending until resolution");
        },
    );
}

#[test]
fn issue_4362_resource_reference_admission_uses_anonymous_overlap() {
    let string = |min, max, member, pattern: &str| {
        let pattern = Some(crate::schema::PatternSpec::new(
            pattern,
            crate::schema::PatternMatch::Full,
        ));
        if member {
            AttributeType::string_member(
                crate::schema::StringIdentityLayer::new(None, None, None, None),
                crate::schema::StringMemberConstraints::new(pattern, Some((Some(min), Some(max)))),
            )
        } else {
            AttributeType::refined_string(None, pattern, Some((Some(min), Some(max))), None)
        }
    };
    for (min, max, admitted, member) in [
        (1, 100, true, false),
        (50, 60, false, false),
        (1, 100, true, true),
        (50, 60, false, true),
    ] {
        fixture(
            string(min, max, member, "[a-z]+"),
            string(1, 47, member, "a+"),
            |catalog, schemas, _, _| {
                let directory = tempfile::tempdir().unwrap();
                std::fs::write(
                    directory.path().join("main.crn"),
                    "let producer = test.Source {}\n",
                )
                .unwrap();
                std::fs::write(directory.path().join("consumer.crn"), "let forwarded = producer.output\nattributes { annotated: String = producer.output }\nlet direct = test.Sink { target = producer.output }\nlet via_binding = test.Sink { target = forwarded }\n").unwrap();
                std::fs::write(
                    directory.path().join("exports.crn"),
                    "exports { output: String = producer.output }\n",
                )
                .unwrap();
                let parsed_directory = crate::config_loader::parse_directory(
                    &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
                    catalog,
                )
                .unwrap();
                let parsed = crate::validation::inference::apply_inference(
                    parsed_directory.into_parsed().resolve_root_for_test(),
                    schemas,
                    catalog,
                );
                let bindings = BindingIndex::from_parsed(&parsed, schemas, catalog);
                let configuration =
                    crate::config_loader::load_buffer_configuration(&parsed, schemas, catalog);
                crate::validation::validate_attribute_param_ref_types_with_bindings(
                    &configuration.judged().file().attribute_params,
                    &bindings,
                )
                .unwrap();
                crate::validation::validate_export_param_ref_types_with_bindings(
                    &configuration.judged().file().export_params,
                    &bindings,
                )
                .unwrap();
                let result = crate::validation::validate_resource_ref_types(
                    configuration.judged(),
                    &std::collections::HashSet::new(),
                    &bindings,
                    &mut Vec::new(),
                );
                assert_eq!(
                    result.is_ok(),
                    admitted,
                    "String({min}..={max}) -> String(1..=47), member={member}: {result:?}"
                );
                if admitted {
                    with_session_for_test(catalog, |session| {
                        for name in ["direct", "via_binding"] {
                            let resource = parsed
                                .resources
                                .iter()
                                .find(|resource| {
                                    resource
                                        .binding
                                        .as_ref()
                                        .is_some_and(|binding| binding.as_str() == name)
                                })
                                .unwrap();
                            let destination = schemas
                                .resource_attribute_destination(&resource.id, "target", catalog)
                                .unwrap();
                            let value = resource.attributes["target"].to_expanded().expect(
                                "forwarded resource references have no parser placeholders",
                            );
                            let pending = validate(&destination, &value, &bindings, session)
                                .unwrap()
                                .ready()
                                .unwrap_err();
                            assert!(pending.local_obligation().is_some());
                            for (resolved_value, accepted, pattern_error) in [
                                ("a".repeat(32), true, false),
                                ("a".repeat(48), false, false),
                                ("z".repeat(32), false, true),
                            ] {
                                let mut resolved = ResolvedBindings::default();
                                resolved.set(
                                    "producer",
                                    HashMap::from([("output".into(), text(&resolved_value))]),
                                    BindingValueSource::Local,
                                );
                                let result = pending.clone().discharge(&resolved, session);
                                assert_eq!(
                                    result.is_ok(),
                                    accepted,
                                    "{name}, value={resolved_value}, member={member}: {result:?}"
                                );
                                if let Err(error) = result {
                                    let ResolutionError::AdmissionConstraints(findings) =
                                        error.resolution_error()
                                    else {
                                        panic!("destination value check: {error:?}");
                                    };
                                    assert!(
                                        findings.findings().iter().any(
                                            |finding| if pattern_error {
                                                matches!(
                                                    finding.error,
                                                    TypeError::PatternMismatch { .. }
                                                )
                                            } else {
                                                matches!(
                                                    finding.error,
                                                    TypeError::LengthOutOfRange { .. }
                                                )
                                            }
                                        ),
                                        "{name}: {error:?}"
                                    );
                                }
                            }
                        }
                    });
                }
                if let Err(error) = result {
                    let message = error.to_string();
                    // Original anonymous evidence is skipped by IdentifiedLeaves;
                    // the forwarding declaration's Full check must still reject.
                    for name in ["direct", "via_binding"] {
                        assert!(
                            message.contains(&format!("test.Sink.{name}: cannot assign")),
                            "{message}"
                        );
                    }
                }
            },
        );
    }
}

#[test]
fn issue_4362_sibling_destinations_retain_anonymous_value_checks() {
    fn check<K: RechecksResolved + Clone + std::fmt::Debug>(
        destination: &Destination<K>,
        catalog: &ProviderContext,
        bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
        admitted: bool,
    ) {
        with_session_for_test(catalog, |session| {
            let result = validate(destination, &reference(), bindings, session);
            if !admitted {
                assert!(
                    result.is_err(),
                    "disjoint bounds must reject for {}",
                    std::any::type_name::<K>()
                );
                return;
            }
            let pending = result
                .expect("overlapping anonymous bounds defer")
                .ready()
                .unwrap_err();
            assert!(pending.local_obligation().is_some());
            for (length, accepted) in [(32, true), (48, false)] {
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("output".into(), text(&"a".repeat(length)))]),
                    BindingValueSource::Local,
                );
                let result = pending.clone().discharge(&resolved, session);
                assert_eq!(
                    result.is_ok(),
                    accepted,
                    "{} length={length}: {result:?}",
                    std::any::type_name::<K>()
                );
            }
        });
    }
    for (min, max, admitted) in [(1, 100, true), (50, 60, false)] {
        fixture(
            AttributeType::refined_string(None, None, Some((Some(min), Some(max))), None),
            AttributeType::refined_string(None, None, Some((Some(1), Some(47))), None),
            |catalog, _, bindings, destination| {
                check(destination, catalog, bindings, admitted);
                check(
                    &Destination::<kind::AnnotatedAttribute> {
                        identity: destination.identity.clone(),
                        label: destination.label.clone(),
                        contract: destination.contract.clone(),
                        kind: PhantomData,
                    },
                    catalog,
                    bindings,
                    admitted,
                );
                check(
                    &Destination::<kind::ModuleArgument> {
                        identity: destination.identity.clone(),
                        label: destination.label.clone(),
                        contract: destination.contract.clone(),
                        kind: PhantomData,
                    },
                    catalog,
                    bindings,
                    admitted,
                );
                check(
                    &Destination::<kind::Export> {
                        identity: destination.identity.clone(),
                        label: destination.label.clone(),
                        contract: destination.contract.clone(),
                        kind: PhantomData,
                    },
                    catalog,
                    bindings,
                    admitted,
                );
            },
        );
    }
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
                let (candidate, mut context) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(bindings),
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
        AttributeType::int_member(
            crate::schema::IntIdentityLayer::new(Some(TypeIdentity::bare("Port")), None),
            crate::schema::IntMemberConstraints::new(Some((Some(0), Some(65535)))),
        ),
        AttributeType::int_member(
            crate::schema::IntIdentityLayer::new(Some(TypeIdentity::bare("Port")), None),
            crate::schema::IntMemberConstraints::new(Some((Some(1), Some(1024)))),
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
fn module_argument_retains_the_same_destination_obligation() {
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
                assert!(
                    validate(&strict, &reference(), bindings, session)
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
                string_slot("Arn", None, Some((Some(1), Some(1600)))),
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
                let placed = pending.discharge(&resolved, session).unwrap();
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
                            wrong.inner.destination.identity.revision =
                                CatalogRevision(CatalogSnapshot::Provider(
                                    ProviderContext::new(crate::EnvironmentSnapshot::empty())
                                        .custom_types,
                                ));
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
    let scalar = string_slot(
        identity,
        uuid.then(|| {
            crate::schema::PatternSpec::new(
                "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
                crate::schema::PatternMatch::Full,
            )
        }),
        Some((Some(1), Some(if uuid { 36 } else { 1600 }))),
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
                assert_eq!(apply.unwrap().value(), &wrap(text(&output)), "{binding}");
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
                let (candidate, _) = Candidate::validation_classified(
                    first
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(bindings),
                    session,
                );
                let (_, context) = Candidate::validation_classified(
                    second
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(bindings),
                    session,
                );
                assert!(admit(destination, candidate, &context).is_err());
            })
        },
    );
}

#[test]
fn pending_is_holdable_and_discharged_for_every_assignment_kind() {
    fn check<K: AssignmentKind + std::fmt::Debug>(
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
            pending.discharge(&resolved, session).unwrap().value(),
            &text("ok")
        );
    }
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                check::<kind::ResourceAttribute>(destination, bindings, session);
                check::<kind::DataSourceAttribute>(destination, bindings, session);
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
    fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, _, _, destination| {
            let mut destination = destination.clone();
            destination.contract = None;
            with_session_for_test(catalog, |session| {
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
        },
    );
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
                    source
                        .constraints()
                        .next()
                        .unwrap()
                        .assignment_to(contract.sink_type(contract.attr(), catalog)),
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

fn partial_catalog_admission_fixture(
    f: impl FnOnce(
        &BindingIndex<'_, AbsoluteBindingAddress>,
        &Destination<kind::ResourceAttribute>,
        &dyn ProviderTypeCheckContext,
    ),
) {
    let mut catalog = crate::parser::LoadedProviderContext::empty_for_test()
        .as_provider_context()
        .clone();
    catalog.custom_types = catalog
        .custom_types
        .with_failed_providers(std::collections::BTreeSet::from(["test".into()]));
    assert!(catalog.custom_types.is_partial());
    fixture_in(
        &catalog,
        string_type("TargetId", None),
        string_type("TargetId", None),
        |_, _, bindings, destination| {
            with_session_for_test(&catalog, |session| f(bindings, destination, session));
        },
    );
}

fn assert_provider_catalog_error(error: AdmissionErrors) {
    let ResolutionError::AdmissionConstraints(findings) = error.resolution_error() else {
        panic!("expected provider custom-type error, got {error:?}");
    };
    assert_eq!(findings.findings().len(), 1);
    let finding = &findings.findings()[0];
    assert_eq!(finding.path.to_string(), "target");
    assert_eq!(finding.error.attribute_name(), Some("target"));
    assert!(matches!(
        &finding.error,
        TypeError::CustomTypeValidation { error, .. }
            if matches!(error.as_ref(), crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
    ));
    assert!(
        finding
            .error
            .to_string()
            .contains("provider custom types are not loaded")
    );
}

#[test]
fn partial_catalog_custom_type_error_at_validation() {
    partial_catalog_admission_fixture(|bindings, destination, session| {
        assert_provider_catalog_error(
            validate(destination, &text("fits"), bindings, session).unwrap_err(),
        );
    });
}

#[test]
fn partial_catalog_custom_type_error_at_plan() {
    partial_catalog_admission_fixture(|_, destination, session| {
        let value = text("fits");
        let resolved = ResolvedBindings::default();
        let proof =
            PlanResolved::try_from(value.knowledge(&KnowledgeContext::plan(&resolved)).unwrap())
                .unwrap();
        let (candidate, context) = Candidate::plan(proof, session);
        assert_provider_catalog_error(admit(destination, candidate, &context).unwrap_err());
    });
}

#[test]
fn partial_catalog_custom_type_error_at_apply() {
    partial_catalog_admission_fixture(|_, destination, session| {
        let value = text("fits");
        let resolved = ResolvedBindings::default();
        let (candidate, context) = Candidate::apply(
            value
                .knowledge(&KnowledgeContext::apply(&resolved))
                .unwrap(),
            session,
        )
        .unwrap();
        assert_provider_catalog_error(admit(destination, candidate, &context).unwrap_err());
    });
}

#[test]
fn partial_catalog_custom_type_error_on_discharge() {
    partial_catalog_admission_fixture(|bindings, destination, session| {
        let pending = validate(destination, &reference(), bindings, session)
            .unwrap()
            .ready()
            .unwrap_err();
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "producer",
            HashMap::from([("output".into(), text("fits"))]),
            BindingValueSource::Local,
        );
        assert_provider_catalog_error(pending.discharge(&resolved, session).unwrap_err());
    });
}

#[test]
fn provider_apply_and_discharge_use_catalog_when_factory_arguments_differ() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let identity = TypeIdentity::bare("RemoteId");
    let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
        Default::default(),
        Box::new(move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(crate::parser::CustomTypeValidationError::Rejected {
                owner: "test".into(),
                identity: TypeIdentity::bare("RemoteId"),
                message: "catalog rejection".into(),
            })
        }),
    );
    fixture_in(
        &catalog,
        string_type("RemoteId", None),
        string_type("RemoteId", None),
        |catalog, schemas, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let pending = validate(destination, &reference(), bindings, session)
                    .unwrap()
                    .ready()
                    .unwrap_err();
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "producer",
                    HashMap::from([("output".into(), text("rejected"))]),
                    BindingValueSource::Local,
                );
                let expected =
                    TypeError::from(crate::parser::CustomTypeValidationError::Rejected {
                        owner: "test".into(),
                        identity,
                        message: "catalog rejection".into(),
                    })
                    .with_attribute("target");
                assert_eq!(
                    pending.discharge(&resolved, session).unwrap_err(),
                    constraints(vec![AdmissionFinding {
                        path: destination.contract.as_deref().unwrap().path.clone(),
                        error: expected.clone(),
                    }])
                );

                let gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
                // The retained catalog owns validation even when a caller's
                // compatibility factory argument is empty or from elsewhere.
                let context = crate::executor::ProviderPreparationContext::for_test(
                    &resolved,
                    &gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    schemas,
                    session,
                );
                let resource =
                    crate::resource::Resource::with_provider("test", "Sink", "sink", None)
                        .with_attribute("target", text("rejected"));
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let error = runtime
                    .block_on(crate::executor::prepare_provider_ready_resource(
                        resource, &context,
                    ))
                    .unwrap_err();
                let crate::executor::ProviderPreparationError::ValueConstraint { errors, .. } =
                    error
                else {
                    panic!("expected catalog rejection, got {error:?}")
                };
                assert_eq!(errors.errors(), &[expected]);
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            });
        },
    );
}

#[test]
fn schema_and_contract_lookups_agree_at_apply_and_discharge() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
        Default::default(),
        Box::new(move |target, value| {
            let crate::parser::CustomTypeValidationTarget::Annotation(identity) = target else {
                panic!("fixture annotation")
            };
            assert_eq!(identity.provider_names().collect::<Vec<_>>(), vec!["test"]);
            counter.fetch_add(1, Ordering::SeqCst);
            if value == &text("rejected") {
                Err(crate::parser::CustomTypeValidationError::Rejected {
                    owner: "test".into(),
                    identity: identity.identity().clone(),
                    message: "provider rejection".into(),
                })
            } else {
                Ok(())
            }
        }),
    );
    for (target, output, accepted, dispatches) in [
        (string_type("RemoteId", None), text("fits"), true, 1),
        (string_type("RemoteId", None), text("rejected"), false, 1),
        (
            AttributeType::refined_float(Some(TypeIdentity::bare("Amount")), None),
            Value::Concrete(crate::resource::ConcreteValue::Int(9_007_199_254_740_993)),
            false,
            0,
        ),
    ] {
        fixture_in(
            &catalog,
            target.clone(),
            target,
            |catalog, schemas, bindings, destination| {
                with_session_for_test(catalog, |session| {
                    let schema = schemas
                        .get("test", "Sink", crate::schema::SchemaKind::Resource)
                        .unwrap();
                    let lookup = crate::parser::custom_type_lookup_for_schema(
                        catalog.custom_type_resolver(),
                        schema,
                    );
                    let contract = destination.contract.as_deref().unwrap();
                    calls.store(0, Ordering::SeqCst);
                    let findings = crate::schema::value_validation::validate_value_with_lookup(
                        contract.attr(),
                        &output
                            .knowledge(&KnowledgeContext::validation())
                            .unwrap()
                            .into_classification(),
                        contract.defs(),
                        contract.ancestry,
                        &lookup,
                    );
                    let expected = findings
                        .into_located()
                        .into_iter()
                        .map(|(path, error)| AdmissionFinding {
                            path: join(&contract.path, &path),
                            error: error.with_attribute("target"),
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(expected.is_empty(), accepted);
                    assert_eq!(calls.load(Ordering::SeqCst), dispatches);

                    let pending = validate(destination, &reference(), bindings, session)
                        .unwrap()
                        .ready()
                        .unwrap_err();
                    let mut resolved = ResolvedBindings::default();
                    resolved.set(
                        "producer",
                        HashMap::from([("output".into(), output.clone())]),
                        BindingValueSource::Local,
                    );
                    let (candidate, boundary) = Candidate::apply(
                        output
                            .knowledge(&KnowledgeContext::apply(&resolved))
                            .unwrap(),
                        session,
                    )
                    .unwrap();
                    let applied: Result<PlacedValue<kind::ResourceAttribute>, AdmissionErrors> =
                        admit_apply(destination, candidate, &boundary);
                    let discharged: Result<PlacedValue<kind::ResourceAttribute>, AdmissionErrors> =
                        pending.discharge(&resolved, session);
                    for result in [applied, discharged] {
                        if accepted {
                            assert_eq!(result.unwrap().value(), &output);
                        } else {
                            assert_eq!(result.unwrap_err(), constraints(expected.clone()));
                        }
                    }
                    assert_eq!(calls.load(Ordering::SeqCst), 3 * dispatches);
                })
            },
        );
    }
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
fn conversion_error_can_be_consumed_through_the_4260_rebase_surface() {
    let source = ResolutionError::Message("original conversion failure".into());
    let error = AdmissionErrors::conversion(source.clone());
    assert_eq!(error.into_resolution_error(), source);
}

#[test]
fn conversion_error_display_label_remains_available_for_4260() {
    let source = ResolutionError::Message("original conversion failure".into());
    let error = AdmissionErrors::conversion(source.clone());
    assert_eq!(error.to_string(), "conversion admission failed");
    assert_eq!(error.resolution_error(), &source);
    assert_eq!(ResolutionError::from(error), source);
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

mod identity_member_obligations {
    use super::*;
    use crate::schema::{
        IntIdentityLayer, IntMemberConstraints, PatternMatch, PatternSpec, StringIdentityLayer,
        StringMemberConstraints, ValidatedConfigSchema,
    };

    fn string(identity: &str, length: Option<(Option<u64>, Option<u64>)>) -> AttributeType {
        AttributeType::string_member(
            StringIdentityLayer::new(Some(TypeIdentity::from_dotted(identity)), None, None, None),
            StringMemberConstraints::new(None, length),
        )
    }

    fn target(identity: &str) -> AttributeType {
        AttributeType::struct_(
            "Target",
            vec![
                StructField::required(
                    "role_arn",
                    AttributeType::string_member(
                        StringIdentityLayer::new(
                            Some(TypeIdentity::from_dotted(identity)),
                            None,
                            None,
                            None,
                        ),
                        StringMemberConstraints::new(
                            Some(PatternSpec::new(
                                "arn:.*:role/[A-Za-z]+",
                                PatternMatch::Full,
                            )),
                            Some((Some(1), Some(1600))),
                        ),
                    ),
                )
                .build(),
            ],
        )
    }

    fn wire(declaration: AttributeType) -> AttributeType {
        let schema = ValidatedConfigSchema::validate(std::collections::BTreeMap::from([(
            "value".into(),
            declaration,
        )]))
        .unwrap();
        let encoded =
            crate::protocol_convert::attribute_type_to_protocol(&schema.parts()["value"]).unwrap();
        let json = serde_json::to_string(&encoded).unwrap();
        crate::protocol_convert::protocol_to_attribute_type(&serde_json::from_str(&json).unwrap())
            .unwrap()
    }

    fn role_arn(value: Value) -> Value {
        Value::Concrete(crate::resource::ConcreteValue::Map(
            indexmap::IndexMap::from([("role_arn".into(), value)]),
        ))
    }

    fn check_destination_obligation(source_identity: &str, decoded: bool) {
        let mut source = string(source_identity, None);
        let mut target = target("aws.Arn");
        if decoded {
            source = wire(source);
            target = wire(target);
        }
        fixture(source, target, |catalog, _, bindings, destination| {
            with_session_for_test(catalog, |session| {
                let long = format!("arn:aws:iam::123456789012:role/{}", "A".repeat(1600));
                for (value, valid) in [
                    ("", false),
                    ("bad", false),
                    (long.as_str(), false),
                    ("arn:aws:iam::123456789012:role/Good", true),
                ] {
                    let pending = validate(destination, &role_arn(reference()), bindings, session)
                        .expect("identity admission must defer unproven member constraints")
                        .ready()
                        .unwrap_err();
                    assert!(pending.local_obligation().is_some());
                    let mut resolved = ResolvedBindings::default();
                    resolved.set(
                        "producer",
                        HashMap::from([("output".into(), text(value))]),
                        BindingValueSource::Local,
                    );
                    let expected = validate(destination, &role_arn(text(value)), bindings, session);
                    assert_eq!(expected.is_ok(), valid, "{value}: {expected:?}");
                    match expected {
                        Ok(_) => {
                            assert!(pending.discharge(&resolved, session).is_ok());
                        }
                        Err(literal_error) => {
                            let error = pending.discharge(&resolved, session).unwrap_err();
                            assert_eq!(error.resolution_error(), literal_error.resolution_error());
                            let ResolutionError::AdmissionConstraints(findings) =
                                error.resolution_error()
                            else {
                                panic!("destination constraint: {error:?}");
                            };
                            assert_eq!(findings.findings().len(), 1);
                            assert_eq!(findings.findings()[0].path.to_string(), "target.role_arn");
                            assert_eq!(findings.to_string().matches("target.role_arn").count(), 1);
                        }
                    }
                }
            });
        });
    }

    #[test]
    fn structural_widening_defers_member_constraints() {
        check_destination_obligation("aws.iam.Role.Arn", false);
    }

    #[test]
    fn same_identity_keeps_the_existing_member_obligation() {
        check_destination_obligation("aws.Arn", false);
    }

    #[test]
    fn decoded_protocol_widening_keeps_the_member_obligation() {
        check_destination_obligation("aws.iam.Role.Arn", true);
    }

    #[test]
    fn unproven_pattern_containment_is_a_destination_obligation() {
        for identity in ["aws.Arn", "aws.iam.Role.Arn"] {
            let source = AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted(identity)),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(
                    Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
                    Some((Some(1), Some(50))),
                ),
            );
            fixture(
                source,
                target("aws.Arn"),
                |catalog, _, bindings, destination| {
                    with_session_for_test(catalog, |session| {
                        assert!(
                            validate(destination, &role_arn(reference()), bindings, session)
                                .unwrap()
                                .ready()
                                .unwrap_err()
                                .local_obligation()
                                .is_some()
                        );
                    });
                },
            );
        }
    }

    #[test]
    fn source_member_lengths_defer_but_disjoint_identity_lengths_reject() {
        for source_identity in ["aws.Arn", "aws.iam.Role.Arn"] {
            fixture(
                string(source_identity, Some((Some(2000), Some(3000)))),
                target("aws.Arn"),
                |catalog, _, bindings, destination| {
                    with_session_for_test(catalog, |session| {
                        assert!(
                            validate(destination, &role_arn(reference()), bindings, session)
                                .unwrap()
                                .ready()
                                .is_err()
                        );
                    });
                },
            );
        }
    }

    #[test]
    fn provably_disjoint_identity_lengths_reject_widening() {
        fixture(
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("aws.iam.Role.Arn")),
                None,
                Some((Some(2000), Some(3000))),
                None,
            ),
            target("aws.Arn"),
            |catalog, _, bindings, destination| {
                with_session_for_test(catalog, |session| {
                    assert!(
                        validate(destination, &role_arn(reference()), bindings, session).is_err()
                    );
                })
            },
        );
    }

    #[test]
    fn a_widening_not_allowed_by_the_graph_still_rejects() {
        for (source, sink) in [
            ("aws.Arn", "aws.iam.Role.Arn"),
            ("aws.iam.Role.Arn", "gcp.Arn"),
            ("aws.iam.Role.Arn", "aws.acm.Certificate.Arn"),
        ] {
            fixture(
                string(source, None),
                target(sink),
                |catalog, _, bindings, destination| {
                    with_session_for_test(catalog, |session| {
                        assert!(
                            validate(destination, &role_arn(reference()), bindings, session)
                                .is_err()
                        );
                    });
                },
            );
        }
    }

    #[test]
    fn int_identity_ranges_reject_disjointness_and_member_ranges_defer() {
        for source_identity in ["aws.Count", "aws.service.Counter.Count"] {
            for (source_range, admitted) in [
                (None, true),
                (Some((Some(0), Some(100))), true),
                (Some((Some(0), Some(10))), true),
                (Some((Some(30), Some(40))), false),
            ] {
                let source = AttributeType::int_member(
                    IntIdentityLayer::new(
                        Some(TypeIdentity::from_dotted(source_identity)),
                        if source_identity == "aws.Count" {
                            None
                        } else {
                            source_range
                        },
                    ),
                    IntMemberConstraints::new(if source_identity == "aws.Count" {
                        source_range
                    } else {
                        None
                    }),
                );
                let target = AttributeType::int_member(
                    IntIdentityLayer::new(Some(TypeIdentity::from_dotted("aws.Count")), None),
                    IntMemberConstraints::new(Some((Some(10), Some(20)))),
                );
                fixture(source, target, |catalog, _, bindings, destination| {
                    with_session_for_test(catalog, |session| {
                        let admission = validate(destination, &reference(), bindings, session);
                        if !admitted && source_identity != "aws.Count" {
                            assert!(admission.is_err());
                            return;
                        }
                        for value in [5, 10] {
                            let pending = validate(destination, &reference(), bindings, session)
                                .expect("overlapping ranges need a value check")
                                .ready()
                                .unwrap_err();
                            let literal =
                                Value::Concrete(crate::resource::ConcreteValue::Int(value));
                            let mut resolved = ResolvedBindings::default();
                            resolved.set(
                                "producer",
                                HashMap::from([("output".into(), literal.clone())]),
                                BindingValueSource::Local,
                            );
                            let expected = validate(destination, &literal, bindings, session);
                            assert_eq!(expected.is_ok(), value == 10, "{expected:?}");
                            match expected {
                                Ok(_) => {
                                    assert!(pending.discharge(&resolved, session).is_ok());
                                }
                                Err(expected) => {
                                    let error = pending.discharge(&resolved, session).unwrap_err();
                                    assert_eq!(
                                        error.resolution_error(),
                                        expected.resolution_error()
                                    );
                                    let ResolutionError::AdmissionConstraints(findings) =
                                        error.resolution_error()
                                    else {
                                        panic!("range finding");
                                    };
                                    assert_eq!(findings.findings().len(), 1);
                                }
                            }
                        }
                    });
                });
            }
        }
    }

    #[test]
    fn explicit_owner_edge_uses_the_same_member_obligation() {
        struct EdgeCatalog(crate::schema::ValidatedProviderSchemaWitness);
        impl crate::parser::CustomTypeResolver for EdgeCatalog {
            fn validate_validated(
                &self,
                _: &str,
                _: crate::schema::LocallyValidatedCustomType<'_>,
            ) -> Result<(), crate::parser::CustomTypeValidationError> {
                Ok(())
            }
        }
        impl crate::parser::ProviderCustomTypeSource for EdgeCatalog {
            fn known_custom_types(
                &self,
            ) -> std::collections::BTreeSet<crate::parser::OwnedTypeIdentity> {
                Default::default()
            }
            fn assignability_closures(
                &self,
            ) -> std::collections::BTreeMap<
                crate::parser::SchemaTypeOwner,
                crate::schema::AssignabilityClosure,
            > {
                std::collections::BTreeMap::from([(
                    crate::parser::SchemaTypeOwner::Provider("test".into()),
                    self.0.assignability_closure().clone(),
                )])
            }
        }
        let source = string("test.NewArn", None);
        let target = target("test.Arn");
        let witness = crate::schema::ValidatedProviderSchemaWitness::new(
            vec![],
            ValidatedConfigSchema::validate(std::collections::BTreeMap::from([
                ("source".into(), source.clone()),
                ("target".into(), target.clone()),
            ]))
            .unwrap(),
            Default::default(),
            vec![
                crate::schema::AssignabilityEdge::new(
                    TypeIdentity::from_dotted("test.NewArn"),
                    TypeIdentity::from_dotted("test.Arn"),
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        catalog.custom_types =
            crate::parser::ProviderCustomTypes::from_source(EdgeCatalog(witness));
        fixture_in(
            &catalog,
            source,
            target,
            |catalog, _, bindings, destination| {
                with_session_for_test(catalog, |session| {
                    let pending = validate(destination, &role_arn(reference()), bindings, session)
                        .expect("an admitted owner edge has the same destination obligation")
                        .ready()
                        .unwrap_err();
                    let mut resolved = ResolvedBindings::default();
                    resolved.set(
                        "producer",
                        HashMap::from([("output".into(), text("bad"))]),
                        BindingValueSource::Local,
                    );
                    assert_eq!(
                        pending
                            .discharge(&resolved, session)
                            .unwrap_err()
                            .resolution_error(),
                        validate(destination, &role_arn(text("bad")), bindings, session)
                            .unwrap_err()
                            .resolution_error()
                    );
                });
            },
        );
    }
}

#[test]
fn every_assignment_kind_retains_the_same_member_obligation() {
    use crate::schema::{PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints};
    let ty = |member| {
        AttributeType::string_member(
            StringIdentityLayer::new(Some(TypeIdentity::bare("Name")), None, None, None),
            member,
        )
    };
    fixture(
        ty(StringMemberConstraints::new(
            None,
            Some((Some(1), Some(128))),
        )),
        ty(StringMemberConstraints::new(
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            Some((Some(3), Some(6))),
        )),
        |catalog, _, bindings, resource| {
            fn check<K: AssignmentKind>(
                resource: &Destination<kind::ResourceAttribute>,
                bindings: &BindingIndex<'_, AbsoluteBindingAddress>,
                session: &dyn ProviderTypeCheckContext,
            ) {
                let destination = Destination::<K> {
                    identity: resource.identity.clone(),
                    label: resource.label.clone(),
                    contract: resource.contract.clone(),
                    kind: PhantomData,
                };
                for (value, valid) in [("A", false), ("ab", false), ("abc", true)] {
                    let pending = validate(&destination, &reference(), bindings, session)
                        .expect("identity admission is independent of the destination kind")
                        .ready()
                        .unwrap_err();
                    assert!(pending.local_obligation().is_some());
                    let mut resolved = ResolvedBindings::default();
                    resolved.set(
                        "producer",
                        HashMap::from([("output".into(), text(value))]),
                        BindingValueSource::Local,
                    );
                    let expected = validate(&destination, &text(value), bindings, session);
                    assert_eq!(expected.is_ok(), valid);
                    match expected {
                        Ok(_) => {
                            assert!(pending.discharge(&resolved, session).is_ok());
                        }
                        Err(expected) => {
                            let error = pending.discharge(&resolved, session).unwrap_err();
                            assert_eq!(error.resolution_error(), expected.resolution_error());
                            let ResolutionError::AdmissionConstraints(findings) =
                                error.resolution_error()
                            else {
                                panic!("member failure");
                            };
                            assert_eq!(findings.findings().len(), 1);
                        }
                    }
                }
            }
            with_session_for_test(catalog, |session| {
                check::<kind::ResourceAttribute>(resource, bindings, session);
                check::<kind::DataSourceAttribute>(resource, bindings, session);
                check::<kind::ModuleArgument>(resource, bindings, session);
                check::<kind::AnnotatedAttribute>(resource, bindings, session);
                check::<kind::Export>(resource, bindings, session);
                check::<kind::UpstreamExport>(resource, bindings, session);
            });
        },
    );
}

#[test]
fn round13_cross_owner_same_identity_defers_in_reference_and_declared_admission() {
    use crate::binding_index::ResolvedBindings;
    let arn = TypeIdentity::from_dotted("aws.iam.Role.Arn");
    let source =
        AttributeType::refined_string(Some(arn.clone()), None, Some((Some(1), Some(2048))), None);
    let sink = AttributeType::string_member(
        crate::schema::StringIdentityLayer::new(
            Some(arn),
            None,
            Some((Some(20), Some(2048))),
            None,
        ),
        crate::schema::StringMemberConstraints::new(None, Some((None, Some(1600)))),
    );
    let catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("Values")
            .attribute(AttributeSchema::optional("value0", source).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "aws",
        ResourceSchema::new("Values")
            .attribute(AttributeSchema::optional("value0", sink).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        "let source = awscc.Values {}\nlet target = aws.Values {}\n",
        std::path::Path::new("main.crn"),
        catalog.as_provider_context(),
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(&parsed.resources[1].id, "value0", &catalog)
        .unwrap();
    let source_schema = schemas
        .get("awscc", "Values", crate::schema::SchemaKind::Resource)
        .unwrap();
    let source_type = source_schema
        .effective_attribute_by_authored_name("value0")
        .unwrap();
    let declared_type = crate::parser::TypeExpr::SchemaType {
        owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
            "awscc",
            source_type.field_type(),
        )
        .unwrap(),
    };
    let crate::binding_index::RefType::Typed(source) =
        bindings.ref_type(&crate::resource::AccessPath::root("source", "value0"))
    else {
        panic!("typed source")
    };
    let declared = source
        .with_value_declaration(&declared_type, &bindings)
        .unwrap();
    for as_declared in [false, true] {
        for (length, accepted) in [(10, false), (32, true), (1700, false)] {
            with_session_for_test(&catalog, |session| {
                let value = Value::root_resource_ref("source", "value0", vec![]);
                let (candidate, mut boundary) = Candidate::validation_classified(
                    value
                        .knowledge(&KnowledgeContext::validation())
                        .unwrap()
                        .into_classification(),
                    Some(&bindings),
                    session,
                );
                if as_declared {
                    boundary.declaration = Some(("argument", &declared));
                }
                let pending = admit(&destination, candidate, &boundary)
                    .expect("overlapping same-identity bounds defer")
                    .ready()
                    .unwrap_err();
                assert!(pending.local_obligation().is_some());
                let mut resolved = ResolvedBindings::default();
                resolved.set(
                    "source",
                    HashMap::from([(
                        "value0".into(),
                        Value::Concrete(crate::resource::ConcreteValue::String(
                            "A".repeat(length).into(),
                        )),
                    )]),
                    BindingValueSource::Local,
                );
                let result = pending.discharge(&resolved, session);
                assert_eq!(
                    result.is_ok(),
                    accepted,
                    "declared={as_declared}, length={length}: {result:?}"
                );
                if let Err(error) = result {
                    let ResolutionError::AdmissionConstraints(findings) = error.resolution_error()
                    else {
                        panic!("resolved value must fail the destination constraints: {error:?}");
                    };
                    assert_eq!(findings.findings().len(), 1);
                    assert!(matches!(
                        findings.findings()[0].error,
                        TypeError::LengthOutOfRange { .. }
                    ));
                }
            });
        }
    }
}

#[test]
fn round2_plain_reference_skips_expression_callback() {
    use crate::parser::ProviderContext;
    use crate::resource::RelativeBindingName;
    use crate::resource::knowledge::{KnowledgeContext, collect_classified_attributes};
    use crate::validation::admission::{attributes, kind};
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_and_resolve(
        "let source = mock.Source {}\nlet sink = mock.Sink { input = source.output }",
    )
    .unwrap();
    let judged = crate::parser::retained::JudgedFile::new(parsed, &schemas, &config);
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .project_resources()
        .find(|resource| resource.id().resource_type == "Sink")
        .unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &config);
    let values = resource.attributes();
    let classified = collect_classified_attributes(
        values.iter().map(|(name, value)| (name.clone(), value)),
        &KnowledgeContext::validation(),
        |_, error| panic!("{error}"),
    );
    let actual: Vec<String> =
        crate::validation::deferred_types::with_session_for_test(&config, |session| {
            let check =
                |_: &str,
                 _: &crate::schema::CompiledAttributeType,
                 _: &crate::resource::Value<
                    crate::resource::ParserPlaceholder,
                    RelativeBindingName,
                >,
                 _: &crate::schema::LeafPath,
                 _: &crate::validation::admission::DestinationContract,
                 _: &crate::validation::admission::assignment::ReferenceContext<
                    '_,
                    RelativeBindingName,
                >,
                 _cursor: crate::parser::retained::AuthoredCursor<'_>| {
                    Err(crate::schema::TypeError::MissingRequired {
                        name: "expression-only callback".into(),
                    })
                };
            let result = attributes::admit_validation_attributes::<kind::ResourceAttribute, _, _>(
                resource.schema(&schemas).unwrap(),
                resource.id(),
                &classified,
                resource.quoted_string_attrs(),
                Some(&bindings),
                session,
                Some(&check),
                None,
            );
            result
                .errors()
                .chain(result.symbolic_errors().map(|(_, _, error)| error))
                .map(ToString::to_string)
                .collect()
        });
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../parser/retained/judgement/round2_main.json"
    ))
    .unwrap();
    let actual = serde_json::json!(actual);
    assert_eq!(actual, expected["plain_reference"]);
}

#[derive(Debug)]
struct Round3HookVisit {
    edge: crate::parser::retained::EdgeRole,
    discarded: bool,
    covered: bool,
    selection: bool,
}

fn round3_admission_hook(source: &str, declaration: AttributeType) -> Vec<Round3HookVisit> {
    use crate::parser::retained::{AuthoredCursor, CursorKind};
    use crate::resource::{ParserPlaceholder, RelativeBindingName};
    use crate::schema::value_validation::CheckValue;

    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", declaration).build())
            .validate()
            .unwrap(),
    );
    let parsed =
        crate::parser::parse_file(source, std::path::Path::new("round3-hook.crn"), &catalog)
            .unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .resources()
        .find(|resource| resource.id().resource_type == "Sink")
        .unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(resource.id(), "input", &catalog)
        .unwrap();
    let visits = std::cell::RefCell::new(Vec::new());
    let seen = std::cell::RefCell::new(std::collections::HashSet::new());
    with_session_for_test(&catalog, |session| {
        let hook = |_: &crate::schema::CompiledAttributeType,
                    value: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                    _: &crate::schema::LeafPath,
                    cursor: AuthoredCursor<'_>| {
            let selection = cursor.is_selection();
            let (usage, covered) = match cursor.kind {
                CursorKind::Authored(usage) => (usage, false),
                CursorKind::CoveredBy(usage) => (usage, true),
                CursorKind::Projected(_) => panic!("authored slot lost its cursor"),
            };
            let discarded = matches!(value, CheckValue::Discarded);
            assert!(
                seen.borrow_mut().insert((
                    usage.node().node_id(),
                    usage.occurrence(),
                    discarded,
                    selection,
                )),
                "hook called twice for one authored position"
            );
            visits.borrow_mut().push(Round3HookVisit {
                edge: usage.edge(),
                discarded,
                covered,
                selection,
            });
            Ok(Default::default())
        };
        let ordinary = |_: &crate::schema::CompiledAttributeType,
                        value: &crate::resource::Value<ParserPlaceholder, RelativeBindingName>,
                        _: &crate::schema::LeafPath,
                        cursor: AuthoredCursor<'_>| {
            assert!(matches!(
                assignment::CandidateOrigin::of(value),
                assignment::CandidateOrigin::Expression
            ));
            assert!(!cursor.is_selection());
            let (node, usage, _, _) = cursor.identity().unwrap();
            assert!(
                seen.borrow().contains(&(node, usage, false, false)),
                "the authored hook must precede ordinary checks"
            );
            Ok(Default::default())
        };
        let untyped = Vec::new;
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let context = context.with_checks(
            AssignmentChecks {
                authored: Some(&hook),
                ordinary: Some(&ordinary),
            },
            &untyped,
        );
        let result = admit(&destination, candidate, &context);
        assert!(result.is_ok(), "main accepts this no-as input: {result:?}");
    });
    visits.into_inner()
}

#[test]
fn round3_admission_hook_receives_authored_literals_in_containers() {
    use crate::parser::retained::EdgeRole;
    for (expression, declaration, edge) in [
        ("'value'", AttributeType::string(), EdgeRole::SlotRoot),
        (
            "['value']",
            AttributeType::list(AttributeType::string()),
            EdgeRole::ListItem,
        ),
        (
            "{ key = 'value' }",
            AttributeType::map(AttributeType::string()),
            EdgeRole::MapValue,
        ),
        (
            "{ field = 'value' }",
            AttributeType::struct_(
                "Entry",
                vec![StructField::required("field", AttributeType::string()).build()],
            ),
            EdgeRole::StructField,
        ),
    ] {
        let visits = round3_admission_hook(
            &format!("mock.Sink {{ input = {expression} }}"),
            declaration,
        );
        assert!(
            visits.iter().any(|visit| visit.edge == edge
                && !visit.covered
                && !visit.discarded
                && !visit.selection),
            "{visits:?}"
        );
    }
}

#[test]
fn round3_admission_hook_receives_reference_origins() {
    let visits = round3_admission_hook(
        "let source = mock.Source {}\nmock.Sink { input = source.output }",
        AttributeType::string(),
    );
    assert_eq!(visits.len(), 1, "{visits:?}");
    assert_eq!(visits[0].edge, crate::parser::retained::EdgeRole::SlotRoot);
    assert!(!visits[0].covered);
}

fn round3_assert_alternatives(expression: &str, edge: crate::parser::retained::EdgeRole) {
    let visits = round3_admission_hook(
        &format!("mock.Sink {{ input = {expression} }}"),
        AttributeType::string(),
    );
    assert!(visits.iter().any(|visit| visit.selection), "{visits:?}");
    for discarded in [false, true] {
        assert!(
            visits
                .iter()
                .any(|visit| visit.edge == edge && visit.discarded == discarded && !visit.covered),
            "{visits:?}"
        );
    }
}

#[test]
fn round3_admission_hook_receives_discarded_if_alternative() {
    round3_assert_alternatives(
        "if true { 'live' } else { 'dead' }",
        crate::parser::retained::EdgeRole::IfBranch,
    );
}

#[test]
fn round3_admission_hook_receives_discarded_coalesce_alternative() {
    round3_assert_alternatives(
        "'live' ?? 'dead'",
        crate::parser::retained::EdgeRole::CoalesceOperand,
    );
}

#[test]
fn round3_admission_hook_receives_covered_positions() {
    let visits = round3_admission_hook("mock.Sink { input = 1 + 2 }", AttributeType::int());
    assert!(visits.iter().any(|visit| visit.covered), "{visits:?}");
}

#[test]
fn round7_hook_error_without_attribute_has_one_diagnostic_owner() {
    use crate::resource::{ParserPlaceholder, RelativeBindingName};
    use crate::schema::value_validation::{AttributeAdmission, CheckValue};
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_and_resolve("mock.Sink { input = 'value' }").unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let attributes = indexmap::IndexMap::from([("input".into(), proof)]);
    let destination = schemas
        .resource_attribute_destination(resource.id(), "input", &catalog)
        .unwrap();
    let report = with_session_for_test(&catalog, |session| {
        let hook = |_: &crate::schema::CompiledAttributeType,
                    _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                    _: &crate::schema::LeafPath,
                    _: crate::parser::retained::AuthoredCursor<'_>| {
            Err(crate::schema::TypeError::MissingRequired {
                name: "hook finding".into(),
            })
        };
        resource
            .schema(&schemas)
            .unwrap()
            .schema()
            .as_schema()
            .validate_admission_report(
                &attributes,
                attributes.iter(),
                &|_| false,
                &AttributeAdmission {
                    known: &|_, _, proof| {
                        let (candidate, context) =
                            Candidate::validation_slot(slot, proof.clone(), None, session).unwrap();
                        let untyped = Vec::new;
                        let context = context.with_checks(
                            AssignmentChecks {
                                authored: Some(&hook),
                                ordinary: None,
                            },
                            &untyped,
                        );
                        let mut error = admit(&destination, candidate, &context).unwrap_err();
                        error.take_findings().unwrap()
                    },
                    unknown: None,
                },
            )
    });
    assert_eq!(report.symbolic_errors().count(), 1);
    let errors = report.diagnostic_errors().collect::<Vec<_>>();
    assert_eq!(errors.len(), 1);
    let (error, path, coverage) = errors[0];
    assert!(
        error.attribute_name().is_none(),
        "owner cannot be inferred from error text"
    );
    assert_eq!(path.unwrap().0, "input");
    assert!(coverage.owns_path(), "CLI already suppresses this echo");
    assert!(
        coverage.owns_finding(),
        "LSP must suppress the same hook finding"
    );
}
