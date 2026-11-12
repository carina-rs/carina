use super::*;

fn identity(name: &str) -> AttributeType {
    AttributeType::refined_string(Some(TypeIdentity::from_dotted(name)), None, None, None)
}

fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    for (name, ty) in [
        ("Source", AttributeType::string()),
        ("Other", identity("mock.Other")),
        (
            "Reference",
            AttributeType::reference(TypeIdentity::from_dotted("mock.OtherReference"), None, None),
        ),
        ("Identity", identity("mock.RecoveryProbe")),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(AttributeSchema::optional("name", ty.clone()).build())
                .attribute(AttributeSchema::provider_populated("computed", ty).build())
                .validate()
                .unwrap(),
        );
    }
    for data in [false, true] {
        let schema = ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("value", identity("mock.RecoveryProbe")).build());
        schemas.insert(
            "mock",
            if data {
                schema.as_data_source()
            } else {
                schema
            }
            .validate()
            .unwrap(),
        );
    }
    schemas.insert(
        "mock",
        ResourceSchema::new("ReferenceSink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::reference(
                        TypeIdentity::from_dotted("mock.TargetReference"),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

fn check(source: &str, consumer: &str) -> Vec<String> {
    check_with_schemas(source, consumer, &schemas())
}

fn check_with_schemas(source: &str, consumer: &str, schemas: &SchemaRegistry) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("sources.crn", source),
        ("main.crn", consumer),
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ] {
        std::fs::write(dir.path().join(name), body).unwrap();
    }
    let loaded_catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let catalog = loaded_catalog.as_provider_context();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, catalog)
        .unwrap()
        .into_parsed();
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, schemas, catalog);
    let bindings = BindingIndex::from_parsed(loaded.judged().file(), schemas, catalog);
    crate::validation::validate_resource_admissions(
        loaded.judged(),
        &bindings,
        &std::collections::HashSet::from(["mock".into()]),
        &mut Vec::new(),
    )
    .err()
    .map(|error| error.to_string().lines().map(str::to_owned).collect())
    .unwrap_or_default()
}

#[test]
fn substituted_concrete_references_do_not_add_local_or_schema_deferrals() {
    use crate::schema::{AttrTypeKind, StringIdentityLayer, StringMemberConstraints, StructField};
    let unloaded = AttributeType {
        kind: AttrTypeKind::UnloadedSchemaType(
            TypeIdentity::from_dotted("missing.Value")
                .try_into()
                .unwrap(),
        ),
    };
    let invalid = ResourceSchema::new("UnloadedSource")
        .attribute(AttributeSchema::optional("name", unloaded).build())
        .validate();
    assert!(
        invalid
            .unwrap_err()
            .to_string()
            .contains("schema is not loaded")
    );
    let union = AttributeType::union(vec![
        AttributeType::struct_(
            "First",
            vec![StructField::required("field", AttributeType::string()).build()],
        ),
        AttributeType::struct_(
            "Second",
            vec![StructField::required("field", AttributeType::string()).build()],
        ),
    ]);
    let constrained = AttributeType::string_member(
        StringIdentityLayer::new(None, None, None, None),
        StringMemberConstraints::new(None, Some((None, Some(8)))),
    );
    for (name, source_type, source_value, expression, sink_type) in [
        (
            "unchecked postfix",
            union,
            "{ field = 'abc' }",
            "a.name.field",
            AttributeType::string(),
        ),
        (
            "needs local",
            AttributeType::string(),
            "'abc'",
            "a.name",
            constrained,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        for (file, body) in [
            ("providers.crn", "provider mock {}\n".into()),
            (
                "backend.crn",
                "backend local { path = 'state.json' }\n".into(),
            ),
            (
                "sources.crn",
                format!("let a = mock.Source {{ name = {source_value} }}\n"),
            ),
            (
                "main.crn",
                format!("let sink = mock.Sink {{ value = {expression} }}\n"),
            ),
        ] {
            std::fs::write(directory.path().join(file), body).unwrap();
        }
        let mut schemas = SchemaRegistry::new();
        for (resource, attribute, ty) in [
            ("Source", "name", source_type),
            ("Sink", "value", sink_type),
        ] {
            schemas.insert(
                "mock",
                ResourceSchema::new(resource)
                    .attribute(AttributeSchema::optional(attribute, ty).build())
                    .validate()
                    .unwrap(),
            );
        }
        let catalog = crate::parser::LoadedProviderContext::empty_for_test();
        let catalog = catalog.as_provider_context();
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let parsed = crate::config_loader::parse_directory(&path, catalog)
            .unwrap()
            .into_parsed();
        let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, catalog);
        let judged = loaded.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree
            .project_resources()
            .find(|resource| resource.id().resource_type == "Sink")
            .unwrap();
        let slot = resource.authored_slot("value").unwrap();
        let projected = slot
            .projection()
            .unwrap()
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let bindings = BindingIndex::from_parsed(judged.file(), &schemas, catalog);
        if name == "unchecked postfix" {
            // Postfix access through a union currently stays pending and
            // cannot enter the substituted-known-value policy.
            assert!(!matches!(
                projected,
                crate::resource::knowledge::Classification::Known(_)
            ));
            let path = crate::resource::AccessPath::with_fields("a", "name", vec!["field".into()]);
            assert!(matches!(
                bindings.postfix_ref_type(&path),
                crate::binding_index::RefType::Unchecked
            ));
            continue;
        }
        assert!(matches!(
            projected,
            crate::resource::knowledge::Classification::Known(_)
        ));
        let proof = projected;
        let destination = schemas
            .resource_attribute_destination(resource.id(), "value", catalog)
            .unwrap();
        with_session_for_test(catalog, |session| {
            let (baseline, baseline_context) =
                Candidate::validation_classified(proof.clone(), Some(&bindings), session);
            let baseline =
                assignment::judge_with_findings(&destination, &baseline, &baseline_context)
                    .unwrap();
            assert!(
                matches!(baseline.verdict, AssignmentVerdict::Proven(_)),
                "main concrete path: {name}"
            );
            let (candidate, context) =
                Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
            let actual =
                assignment::judge_with_findings(&destination, &candidate, &context).unwrap();
            assert!(
                matches!(actual.verdict, AssignmentVerdict::Proven(_)),
                "substituted {name} must keep the main concrete verdict"
            );
        });
    }
}

fn with_ordinary_check(
    source_type: &str,
    expression: &str,
    ordinary: super::super::OrdinaryAssignmentCheck<
        '_,
        crate::resource::ParserPlaceholder,
        crate::resource::RelativeBindingName,
    >,
) -> Vec<(LeafPath, TypeError)> {
    let directory = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("providers.crn", "provider mock {}\n".to_owned()),
        (
            "backend.crn",
            "backend local { path = 'state.json' }\n".to_owned(),
        ),
        (
            "sources.crn",
            format!("let a = mock.{source_type} {{ name = 'abc' }}\n"),
        ),
        (
            "main.crn",
            format!("let sink = mock.Sink {{ value = {expression} }}\n"),
        ),
    ] {
        std::fs::write(directory.path().join(name), body).unwrap();
    }
    let schemas = schemas();
    let loaded_catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let catalog = loaded_catalog.as_provider_context();
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, catalog)
        .unwrap()
        .into_parsed();
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, catalog);
    let judged = loaded.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .project_resources()
        .find(|r| r.id().resource_type == "Sink")
        .unwrap();
    let slot = resource.authored_slot("value").unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, catalog);
    let destination = schemas
        .resource_attribute_destination(resource.id(), "value", catalog)
        .unwrap();
    with_session_for_test(catalog, |session| {
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let untyped = Vec::new;
        let context = context.with_checks(
            AssignmentChecks {
                authored: None,
                ordinary: Some(ordinary),
            },
            &untyped,
        );
        assignment::judge_with_findings(&destination, &candidate, &context)
            .err()
            .unwrap()
            .findings
            .unwrap()
            .into_located()
    })
}

#[test]
fn substituted_reference_ordinary_err_keeps_main_single_location() {
    let ordinary =
        |_: &CompiledAttributeType,
         _: &Value<crate::resource::ParserPlaceholder, crate::resource::RelativeBindingName>,
         _: &LeafPath,
         _: crate::parser::retained::AuthoredCursor<'_>| {
            Err(TypeError::ValidationFailed {
                message: "ordinary failure".into(),
            })
        };
    let literal = with_ordinary_check("Identity", "'abc'", &ordinary);
    assert_eq!(literal.len(), 1);
    for source in ["Identity", "Source"] {
        let actual = with_ordinary_check(source, "a.name", &ordinary);
        assert_eq!(
            actual
                .iter()
                .filter(|(_, error)| error == &literal[0].1)
                .count(),
            1,
            "{actual:#?}"
        );
        assert_eq!(
            actual.len(),
            1 + usize::from(source == "Source"),
            "{actual:#?}"
        );
    }
}

#[test]
fn substituted_reference_deduplicates_a_non_first_ordinary_error() {
    let reference = with_ordinary_check("Source", "a.name", &|_, _, _, _| Ok(Default::default()));
    assert_eq!(reference.len(), 1);
    let reference_error = reference[0].1.clone();
    let actual = with_ordinary_check("Source", "a.name", &|_, _, _, _| {
        Ok(crate::schema::value_validation::AssignmentFindings {
            errors: vec![
                (
                    LeafPath::from_segments([]),
                    TypeError::ValidationFailed {
                        message: "first ordinary error".into(),
                    },
                ),
                (LeafPath::from_segments([]), reference_error.clone()),
            ],
            ..Default::default()
        })
    });
    assert_eq!(
        actual
            .iter()
            .filter(|(_, error)| error == &reference_error)
            .count(),
        1,
        "{actual:#?}"
    );
}

#[test]
fn substituted_reference_retains_main_value_constraint_diagnostics() {
    use crate::schema::{PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints};
    let mut schemas = schemas();
    for (name, member) in [
        (
            "PatternSink",
            StringMemberConstraints::new(Some(PatternSpec::new("abc", PatternMatch::Full)), None),
        ),
        (
            "LengthSink",
            StringMemberConstraints::new(None, Some((None, Some(2)))),
        ),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(
                    AttributeSchema::optional(
                        "value",
                        AttributeType::string_member(
                            StringIdentityLayer::new(
                                Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
                                None,
                                None,
                                None,
                            ),
                            member,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        for source in ["Identity", "Source"] {
            let source = format!("let a = mock.{source} {{ name = 'bad' }}\n");
            let literal = check_with_schemas(
                &source,
                &format!("let sink = mock.{name} {{ value = 'bad' }}\n"),
                &schemas,
            );
            assert_eq!(literal.len(), 1, "{literal:#?}");
            let substituted = check_with_schemas(
                &source,
                &format!("let sink = mock.{name} {{ value = a.name }}\n"),
                &schemas,
            );
            assert_eq!(
                substituted
                    .iter()
                    .filter(|finding| *finding == &literal[0])
                    .count(),
                1,
                "{source}, {name}: {substituted:#?}; main literal: {literal:#?}"
            );
            let unknown = check_with_schemas(
                &source,
                &format!("let sink = mock.{name} {{ value = a.computed }}\n"),
                &schemas,
            );
            assert_eq!(
                substituted.len(),
                literal.len() + unknown.len(),
                "{substituted:#?}"
            );
            for reference in unknown {
                assert_eq!(
                    substituted
                        .iter()
                        .filter(|finding| **finding == reference.replace("a.computed", "a.name"))
                        .count(),
                    1,
                    "{substituted:#?}"
                );
            }
        }
    }
}

#[test]
fn substituted_reference_deduplicates_the_same_scalar_kind_failure() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        ResourceSchema::new("IntSource")
            .attribute(AttributeSchema::optional("name", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let source = "let a = mock.IntSource { name = 7 }\n";
    let literal = check_with_schemas(source, "let sink = mock.Sink { value = 7 }\n", &schemas);
    assert_eq!(literal.len(), 1, "{literal:#?}");
    assert_eq!(
        check_with_schemas(
            source,
            "let sink = mock.Sink { value = a.name }\n",
            &schemas
        ),
        literal
    );
}

#[test]
fn substituted_reference_keeps_union_value_errors_and_one_source_error() {
    use crate::schema::{PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints};
    let mut schemas = schemas();
    let member = |identity, pattern| {
        AttributeType::string_member(
            StringIdentityLayer::new(Some(TypeIdentity::from_dotted(identity)), None, None, None),
            StringMemberConstraints::new(Some(PatternSpec::new(pattern, PatternMatch::Full)), None),
        )
    };
    schemas.insert(
        "mock",
        ResourceSchema::new("UnionSink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::union(vec![
                        member("mock.First", "abc"),
                        member("mock.Second", "xyz"),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source = "let a = mock.Source { name = 'bad' }\n";
    let literal = check_with_schemas(
        source,
        "let sink = mock.UnionSink { value = 'bad' }\n",
        &schemas,
    );
    let unknown = check_with_schemas(
        source,
        "let sink = mock.UnionSink { value = a.computed }\n",
        &schemas,
    );
    assert_eq!(literal.len(), 1, "{literal:#?}");
    assert_eq!(unknown.len(), 1, "{unknown:#?}");
    let actual = check_with_schemas(
        source,
        "let sink = mock.UnionSink { value = a.name }\n",
        &schemas,
    );
    assert_eq!(actual.len(), 2, "{actual:#?}");
    assert!(actual.contains(&literal[0]), "{actual:#?}");
    assert!(
        actual.contains(&unknown[0].replace("a.computed", "a.name")),
        "{actual:#?}"
    );
}

#[test]
fn substituted_reference_judges_the_declared_union_independently_of_member_trials() {
    use crate::schema::{PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints};
    let mut schemas = schemas();
    let source_type = identity("mock.First");
    schemas.insert(
        "mock",
        ResourceSchema::new("FirstSource")
            .attribute(AttributeSchema::optional("name", source_type.clone()).build())
            .attribute(AttributeSchema::provider_populated("computed", source_type).build())
            .validate()
            .unwrap(),
    );
    let member = |name, pattern| {
        AttributeType::string_member(
            StringIdentityLayer::new(Some(TypeIdentity::from_dotted(name)), None, None, None),
            StringMemberConstraints::new(Some(PatternSpec::new(pattern, PatternMatch::Full)), None),
        )
    };
    schemas.insert(
        "mock",
        ResourceSchema::new("UnionSink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::union(vec![
                        member("mock.First", "abc"),
                        member("mock.Second", "bad"),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source = "let a = mock.FirstSource { name = 'bad' }\n";
    let unknown = check_with_schemas(
        source,
        "let sink = mock.UnionSink { value = a.computed }\n",
        &schemas,
    );
    assert!(unknown.is_empty(), "{unknown:#?}");
    assert!(
        check_with_schemas(
            source,
            "let sink = mock.UnionSink { value = 'bad' }\n",
            &schemas
        )
        .is_empty()
    );
    let known = check_with_schemas(
        source,
        "let sink = mock.UnionSink { value = a.name }\n",
        &schemas,
    );
    assert_eq!(known, unknown);
}

#[test]
fn substituted_reference_retains_the_ordinary_assignment_callback() {
    let schemas = schemas();
    let catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let catalog = catalog.as_provider_context();
    let parsed = crate::parser::parse_and_resolve(
        "let a = mock.Identity { name = 'abc' }\nlet sink = mock.Sink { value = a.name }\n",
    )
    .unwrap();
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, catalog);
    let judged = loaded.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree
        .project_resources()
        .find(|resource| resource.id().resource_type == "Sink")
        .unwrap();
    let slot = resource.authored_slot("value").unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, catalog);
    let destination = schemas
        .resource_attribute_destination(resource.id(), "value", catalog)
        .unwrap();
    let ordinary = |_: &CompiledAttributeType,
                    _: &crate::resource::Value<
        crate::resource::ParserPlaceholder,
        crate::resource::RelativeBindingName,
    >,
                    _: &LeafPath,
                    _: crate::parser::retained::AuthoredCursor<'_>| {
        Err(TypeError::ValidationFailed {
            message: "ordinary validation finding".into(),
        })
    };
    with_session_for_test(catalog, |session| {
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let untyped = Vec::new;
        let context = context.with_checks(
            AssignmentChecks {
                authored: None,
                ordinary: Some(&ordinary),
            },
            &untyped,
        );
        let error = assignment::judge_with_findings(&destination, &candidate, &context)
            .err()
            .expect("ordinary callback must retain its rejection");
        let findings = error.findings.unwrap();
        let errors: Vec<_> = findings
            .symbolic_errors()
            .map(|(_, error)| error.to_string())
            .collect();
        assert_eq!(errors, ["Validation failed: ordinary validation finding"]);
    });
}

#[test]
fn substituted_reference_source_always_carries_its_authored_use_identity() {
    let directory = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", "let a = mock.Source { name = 'abc' }\n"),
        (
            "main.crn",
            "let copies = for x in [0, 1] { mock.Sink { value = a.name } }\n",
        ),
    ] {
        std::fs::write(directory.path().join(name), body).unwrap();
    }
    let schemas = schemas();
    let loaded_catalog = crate::parser::LoadedProviderContext::empty_for_test();
    let catalog = loaded_catalog.as_provider_context();
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, catalog)
        .unwrap()
        .into_parsed();
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, catalog);
    let judged = loaded.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let mut identities = std::collections::HashSet::new();
    for resource in tree
        .project_resources()
        .filter(|resource| resource.id().resource_type == "Sink")
    {
        let slot = resource.authored_slot("value").unwrap();
        let cursor = crate::parser::retained::AuthoredCursor::authored(slot.cursor().unwrap());
        let source = cursor.source_reference().unwrap();
        assert_eq!(Some(source.identity), cursor.identity());
        assert!(identities.insert(source.identity));
        assert_eq!(source.into_path().as_ref().to_dot_string(), "a.name");
        assert!(cursor.covered().source_reference().is_none());
    }
    assert_eq!(identities.len(), 2);
}

#[test]
fn substituted_reference_known_and_unknown_sources_use_the_same_rule() {
    for source in ["Source", "Other", "Reference", "Identity"] {
        for sink in ["Sink", "ReferenceSink"] {
            for sibling in [false, true] {
                let consumer = format!("let sink = mock.{sink} {{ value = a.name }}\n");
                let run = |value: &str| {
                    let source = format!("let a = mock.{source} {{ {value} }}\n");
                    if sibling {
                        check(&source, &consumer)
                    } else {
                        check("", &format!("{source}{consumer}"))
                    }
                };
                let unknown = run("");
                let accepted =
                    sink == "ReferenceSink" || matches!(source, "Reference" | "Identity");
                assert_eq!(
                    unknown.is_empty(),
                    accepted,
                    "{source} -> {sink}: {unknown:#?}"
                );
                let known = run("name = 'abc'");
                assert_eq!(known, unknown, "{source} -> {sink}, sibling={sibling}");
            }
        }
    }
}

#[test]
fn substituted_reference_matches_the_unknown_computed_source_control() {
    let source = "let a = mock.Source { name = 'abc' }\n";
    let unknown = check(source, "let sink = mock.Sink { value = a.computed }\n");
    assert_eq!(unknown.len(), 1, "{unknown:#?}");
    let known = check(source, "let sink = mock.Sink { value = a.name }\n");
    let expected: Vec<_> = unknown
        .iter()
        .map(|message| message.replace("a.computed", "a.name"))
        .collect();
    assert_eq!(known, expected);
    assert!(!known[0].contains(" as "));
}

#[test]
fn substituted_reference_known_and_unknown_reference_sources_stay_accepted() {
    for value in ["", "name = 'abc'"] {
        assert!(
            check(
                &format!("let a = mock.Reference {{ {value} }}\n"),
                "let sink = mock.Sink { value = a.name }\n",
            )
            .is_empty()
        );
    }
}

#[test]
fn substituted_reference_same_file_and_alias_preserve_the_source() {
    for value in ["a.name", "alias", "(a.name)"] {
        let run = |source| {
            check(
                "",
                &format!(
                    "let a = mock.Source {{ {source} }}\nlet alias = a.name\nlet sink = mock.Sink {{ value = {value} }}\n"
                ),
            )
        };
        let unknown = run("");
        assert_eq!(unknown.len(), 1, "{value}: {unknown:#?}");
        assert_eq!(run("name = 'abc'"), unknown, "{value}");
    }
}

#[test]
fn substituted_reference_each_expansion_keeps_its_own_finding() {
    let errors = check(
        "let a = mock.Source { name = 'abc' }\n",
        "let copies = for _ in [1, 2] { mock.Sink { value = a.name } }\n",
    );
    assert_eq!(errors.len(), 2, "{errors:#?}");
    for index in 0..2 {
        assert!(
            errors
                .iter()
                .any(|error| error.contains(&format!("copies[{index}]"))),
            "{errors:#?}"
        );
    }
}

#[test]
fn substituted_reference_data_source_uses_resource_admission() {
    let run = |source| {
        check(
            &format!("let a = mock.Source {{ {source} }}\n"),
            "let sink = read mock.Sink { value = a.name }\n",
        )
    };
    let unknown = run("");
    assert_eq!(unknown.len(), 1, "{unknown:#?}");
    assert_eq!(run("name = 'abc'"), unknown);
}

#[test]
fn substituted_reference_literals_and_literal_aliases_stay_accepted() {
    for sink in ["Sink", "ReferenceSink"] {
        for value in ["'abc'", "literal", "alias"] {
            assert!(
                check(
                    "let literal = 'abc'\nlet alias = literal\n",
                    &format!("mock.{sink} {{ value = {value} }}\n"),
                )
                .is_empty(),
                "{sink}: {value}"
            );
        }
    }
}

#[test]
fn substituted_reference_discarded_sources_stay_unchecked() {
    for value in [
        "'abc' ?? a.name",
        "if true { 'abc' } else { a.name }",
        "('abc' ?? a.name) ?? a.computed",
    ] {
        let errors = check(
            "let a = mock.Source { name = 'abc' }\n",
            &format!("mock.Sink {{ value = {value} }}\n"),
        );
        assert!(errors.is_empty(), "{value}: {errors:#?}");
    }
}
