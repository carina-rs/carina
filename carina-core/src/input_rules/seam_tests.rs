use super::*;
use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment, Resource, State};
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

fn text(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}
fn registry(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    schemas
}
fn ignore_key(resource: &mut Resource, attribute: &str, key: &str) {
    resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute(attribute.into()),
        ParsedIgnorePathSegment::MapKey(key.into()),
    ])];
}
fn update(
    resource: Resource,
    previous: Value,
    attribute: &str,
    schemas: &SchemaRegistry,
) -> FinalizedPlan {
    let from = State::existing(
        resource.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [(attribute.into(), previous)],
        )),
    )
    .with_identifier("existing");
    let to = crate::resource::ResolvedResource::resolve_raw_fixture(resource, schemas).unwrap();
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to,
        changed_attributes: vec![attribute.into()],
    });
    FinalizedPlan::new(plan)
}

#[test]
fn ignored_default_key_removed_from_exact_patch_is_not_reintroduced() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                InputRuleCode::new("tags.reserved").unwrap(),
                InputPredicate::NoPrefix(crate::schema::NonEmptyString::new("aws:").unwrap()),
            )),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "tags",
        object([("aws:x", text("default")), ("safe", text("new"))]),
    );
    ignore_key(&mut desired, "tags", "aws:x");
    let config = crate::parser::ProviderConfig {
        name: "mock".into(),
        attributes: Default::default(),
        default_tags: IndexMap::from([("aws:x".into(), text("default"))]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let types = InputTypeContext::<ModuleSource>::empty()
        .with_default_tags(&[config], &schemas, [&desired.id])
        .unwrap();
    let finalized = update(desired, object([("safe", text("old"))]), "tags", &schemas);
    let result = finalized.check_inputs_with_types(&schemas, &HashMap::new(), &types);
    assert!(
        result.is_ok(),
        "the exact patch contains only the safe key: {:?}",
        result.err().map(|error| error.errors().to_vec())
    );
}

#[test]
fn ignored_union_instance_is_withheld_before_branch_selection() {
    let local = AttributeType::struct_with_rules(
        "Object",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("object.target").unwrap(),
            InputPredicate::Present,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "objects",
                AttributeType::map(AttributeType::union(vec![local, AttributeType::bool()])),
            )
            .build(),
        ),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "objects",
        object([
            ("selected", object([])),
            ("safe", object([("target", text("new"))])),
        ]),
    );
    ignore_key(&mut desired, "objects", "selected");
    let finalized = update(
        desired,
        object([
            ("selected", Value::Concrete(ConcreteValue::Bool(false))),
            ("safe", object([("target", text("old"))])),
        ]),
        "objects",
        &schemas,
    );
    let checked = finalized.check_inputs(&schemas, &HashMap::new()).unwrap();
    assert_eq!(
        checked.pending_rows(0).len(),
        1,
        "an ignored observed Bool must not suppress possible Struct rules"
    );
}

#[test]
fn erased_root_expression_requires_carried_correspondence() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("name.suffix").unwrap(),
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
            )),
    );
    let authored = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("name", text("old.fifo"));
    let types = InputTypeContext::from_sources(
        &[crate::effect::deps::UnresolvedResource::from_pre_resolve(
            authored.clone(),
        )],
        &[],
        std::iter::empty(),
        &[],
        &schemas,
    );
    let normalized = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "name",
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        crate::resource::ResolvedResource::resolve_raw_fixture(normalized, &schemas).unwrap(),
    ));
    let checked = FinalizedPlan::new(plan)
        .check_inputs_with_types(&schemas, &HashMap::new(), &types)
        .expect("resource identity and attribute spelling do not prove expression correspondence");
    assert_eq!(checked.pending_rows(0).len(), 1);
}

#[test]
fn ignored_missing_child_is_withheld_for_the_walker_and_predicate() {
    let local = AttributeType::struct_with_rules(
        "Child",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("child.target").unwrap(),
            InputPredicate::Present,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::optional("child", local).build(),
                            StructField::optional("changed", AttributeType::string()).build(),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("fallback", AttributeType::string()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("fallback").unwrap(),
                    InputRuleCode::new("fallback.present").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Not(Box::new(
                    InputCondition::Present(InputPath::new("settings.child").unwrap()),
                ))),
            ),
    );
    let mut desired = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "settings",
        object([("child", object([])), ("changed", text("new"))]),
    );
    desired.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute("settings".into()),
        ParsedIgnorePathSegment::Attribute("child".into()),
    ])];
    let finalized = update(
        desired,
        object([("changed", text("old"))]),
        "settings",
        &schemas,
    );
    let checked = finalized
        .check_inputs(&schemas, &HashMap::new())
        .expect("ignored child membership is unavailable");
    assert_eq!(
        checked.pending_rows(0).len(),
        2,
        "both the guard and possible child declaration must retain an obligation: {:?}",
        checked.pending_rows(0)
    );
}

#[test]
fn nested_secret_containers_protect_instantiated_map_keys() {
    let local = AttributeType::struct_with_rules(
        "Object",
        vec![
            StructField::optional("guard", AttributeType::bool()).build(),
            StructField::optional("target", AttributeType::string()).build(),
        ],
        vec![],
        vec![
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("object.target").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Present(InputPath::new("guard").unwrap())),
        ],
    );
    let item = object([(
        "private-key",
        object([("guard", Value::Concrete(ConcreteValue::Bool(true)))]),
    )])
    .into_secret();
    for (ty, value) in [
        (
            AttributeType::list(AttributeType::map(local.clone())),
            Value::Concrete(ConcreteValue::List(vec![item.clone()])),
        ),
        (
            AttributeType::map(AttributeType::map(local)),
            object([("public", item)]),
        ),
    ] {
        let schemas = registry(
            ResourceSchema::new("Review")
                .attribute(AttributeSchema::optional("objects", ty).build()),
        );
        let resource = Resource::with_provider("mock", "Review", "example", None)
            .with_attribute("objects", value);
        let site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                .unwrap()
                .unwrap();
        let findings = site
            .applicable_rules()
            .flat_map(|rule| rule.evaluation().findings)
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1);
        let text = format!("{} {findings:?}", findings[0]);
        assert!(
            !text.contains("private-key"),
            "nested secret map key leaked: {text}"
        );
        assert!(text.contains("<secret>"));
    }
}

fn guarded_tag_keys(ignored: bool) -> InputVerdict {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .input_rule(
                InputRule::new(
                    InputRuleScope::EveryWrite,
                    InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                    InputRuleCode::new("tags.reserved").unwrap(),
                    InputPredicate::NoPrefix(crate::schema::NonEmptyString::new("aws:").unwrap()),
                )
                .with_condition(InputCondition::Equals {
                    path: InputPath::new("guard").unwrap(),
                    literal: InputLiteral::Bool(true),
                }),
            ),
    );
    let tags = if ignored {
        object([("aws:private", text("value"))])
    } else {
        object([])
    };
    let mut desired = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(true)))
        .with_attribute("tags", tags.clone());
    if ignored {
        ignore_key(&mut desired, "tags", "aws:private");
    }
    let previous = State::existing(
        desired.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [
                ("guard".into(), Value::Concrete(ConcreteValue::Bool(false))),
                ("tags".into(), tags),
            ],
        )),
    )
    .with_identifier("existing");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to: crate::resource::ResolvedResource::resolve_raw_fixture(desired, &schemas).unwrap(),
        changed_attributes: vec!["guard".into()],
    });
    let finalized = FinalizedPlan::new(plan);
    let current = HashMap::new();
    let site = finalized
        .write_sites(&schemas, &current)
        .next()
        .unwrap()
        .unwrap();
    site.applicable_rules().next().unwrap().evaluate()
}

#[test]
fn unsent_map_keys_cannot_prove_absent_keys_through_estimation() {
    assert_eq!(guarded_tag_keys(false), InputVerdict::Undecided);
}

#[test]
fn ignored_map_keys_do_not_become_present_key_evidence() {
    assert_eq!(guarded_tag_keys(true), InputVerdict::Undecided);
}

#[test]
fn satisfied_recursive_rules_have_linear_path_work() {
    const COUNT: usize = 64;
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::struct_with_rules(
                    "Node",
                    vec![
                        StructField::optional("target", AttributeType::string()).build(),
                        StructField::optional("child", AttributeType::ref_("Node")).build(),
                    ],
                    vec![],
                    vec![InputRule::value(
                        InputRuleScope::EveryWrite,
                        InputPath::new("target").unwrap(),
                        InputRuleCode::new("node.target").unwrap(),
                        InputPredicate::Present,
                    )],
                ),
            )
            .attribute(AttributeSchema::optional("root", AttributeType::ref_("Node")).build()),
    );
    let mut value = object([("target", text("valid"))]);
    for _ in 1..COUNT {
        value = object([("target", text("valid")), ("child", value)]);
    }
    let resource =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("root", value);
    let site = classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
        .unwrap()
        .unwrap();
    let (_, segments) = crate::schema::with_field_path_work(|| {
        let results = site
            .applicable_rules()
            .map(|rule| rule.evaluation())
            .collect::<Vec<_>>();
        assert_eq!(results.len(), COUNT);
        assert!(
            results
                .iter()
                .all(|result| result.findings.is_empty() && result.pending.is_none())
        );
    });
    assert!(
        segments <= COUNT * 8,
        "satisfied chain materialized {segments} path segments for {COUNT} nodes"
    );
}

#[test]
fn public_ast_mutations_cannot_reseal_authored_input() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("target.required").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Present(InputPath::new("guard").unwrap())),
            ),
    );
    let mut parsed = crate::parser::parse_file(
        "let example = mock.Review { guard = true }",
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let before = check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas).unwrap();
    assert_eq!(before.len(), 1);
    parsed.resources[0].attributes.clear();
    let after = check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas).unwrap();
    assert_eq!(before.findings, after.findings);
    let fabricated = crate::parser::ExpandedParsedFile::default();
    assert!(fabricated.authored_inputs().is_err());
}

#[test]
fn admitted_literal_attribute_names_do_not_panic_input_validation() {
    for rules_elsewhere in [false, true] {
        let mut schema = ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("x-y", AttributeType::string()).build());
        if rules_elsewhere {
            schema = schema.attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_with_rules(
                        "Settings",
                        vec![StructField::optional("target", AttributeType::string()).build()],
                        vec![],
                        vec![InputRule::value(
                            InputRuleScope::EveryWrite,
                            InputPath::new("target").unwrap(),
                            InputRuleCode::new("settings.target").unwrap(),
                            InputPredicate::Present,
                        )],
                    ),
                )
                .build(),
            );
        }
        let schemas = registry(schema);
        let source = if rules_elsewhere {
            "let example = mock.Review { settings = {} }"
        } else {
            "let example = mock.Review {}"
        };
        let parsed = crate::parser::parse_file(
            source,
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let findings = check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas).unwrap();
        assert_eq!(findings.len(), usize::from(rules_elsewhere));
    }
    for name in ["x-y", "a.b"] {
        for supplied in [false, true] {
            check_nested_literal_field(name, supplied);
        }
    }
}

fn check_nested_literal_field(name: &'static str, supplied: bool) {
    let leaf = AttributeType::struct_with_rules(
        "Leaf",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("leaf.target").unwrap(),
            InputPredicate::Present,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::struct_("Settings", vec![StructField::optional(name, leaf).build()]),
            )
            .build(),
        ),
    );
    let settings = if supplied {
        format!("{{ '{name}' = {{}} }}")
    } else {
        "{}".into()
    };
    let parsed = crate::parser::parse_file(
        &format!("let example = mock.Review {{ settings = {settings} }}"),
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let findings = check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas).unwrap();
    assert_eq!(findings.len(), usize::from(supplied), "field {name}");
}

#[test]
fn dotted_nested_schema_field_is_one_literal_lookup() {
    check_nested_literal_field("a.b", false);
    check_nested_literal_field("a.b", true);
}

#[test]
fn all_module_publication_paths_retain_authored_rule_authority() {
    use crate::module_resolver::*;
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        "provider mock { default_tags = m.tags }\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("main.crn"), "let module = use { source = './child' }\nlet m = module {}\nlet example = mock.Review {}\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "attributes { tags = { owner = 'known' } }\n",
    )
    .unwrap();
    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&base, &context).unwrap();
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("tagged.target").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Present(InputPath::new("tags").unwrap())),
            ),
    );
    let results = (0..6)
        .map(|entry| {
            let graph =
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    match entry {
                        0 => resolve_modules_with_config(parsed.clone(), &base, session, &schemas),
                        1 => resolve_modules_with_providerless_config(
                            parsed.clone(),
                            &base,
                            session,
                            &schemas,
                        ),
                        2 => resolve_module_template_with_config(
                            parsed.clone(),
                            &base,
                            session,
                            &schemas,
                        ),
                        3 => resolve_module_template_with_providerless_config(
                            parsed.clone(),
                            &base,
                            session,
                            &schemas,
                        ),
                        4 => resolve_upstream_modules_with_config(
                            parsed.clone(),
                            &base,
                            session,
                            &schemas,
                            &Default::default(),
                        ),
                        5 => resolve_upstream_modules_with_providerless_config(
                            parsed.clone(),
                            &base,
                            session,
                            &schemas,
                            &Default::default(),
                        ),
                        _ => unreachable!(),
                    }
                })
                .unwrap();
            graph
                .authored_inputs()
                .map(|inputs| check_authored_inputs(inputs, &schemas).unwrap().len())
        })
        .collect::<Vec<_>>();
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    for result in results {
        assert_eq!(result.unwrap(), 1);
    }
}

#[test]
fn withheld_nested_map_keys_cannot_inspect_the_ancestor_expression() {
    let reference = |binding: &str, attribute: &str| {
        Value::Deferred(DeferredValue::ResourceRef {
            path: crate::resource::AccessPath::from_binding(
                crate::resource::absolute_binding_address_for_test(binding),
                attribute,
                vec![],
            ),
        })
    };
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![
                            StructField::optional(
                                "labels",
                                AttributeType::map(AttributeType::string()),
                            )
                            .build(),
                        ],
                    ),
                )
                .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("settings.labels").unwrap()),
                InputRuleCode::new("labels.prefix").unwrap(),
                InputPredicate::NoPrefix(crate::schema::NonEmptyString::new("labels").unwrap()),
            )),
    );
    let output = object([("labels", object([("safe", reference("producer", "name"))]))]);
    let types = InputTypeContext::<ModuleSource>::from_sources(
        &[],
        &[],
        [(
            crate::resource::absolute_binding_address_for_test("m"),
            IndexMap::from([("settings".into(), output.clone())]),
        )],
        &[],
        &schemas,
    );
    for (settings, expected) in [
        (reference("m", "settings"), InputVerdict::Undecided),
        (output, InputVerdict::Proven),
    ] {
        let resource = Resource::with_provider("mock", "Review", "example", None)
            .with_attribute("settings", settings);
        let authored = AuthoredInputs::resource_for_test(&resource);
        let schema = schemas.get_for(&resource).unwrap().schema();
        let input = RuleInput::for_authored(
            &authored.resources().next().unwrap(),
            schema,
            Arc::new(types.facts.clone()),
            &schemas,
        )
        .unwrap();
        let site = classify_write_site::<ModuleSource>(
            WriteSiteSource::Preview {
                resource_id: resource.id,
                input,
            },
            &schemas,
        )
        .unwrap()
        .unwrap();
        assert_eq!(site.applicable_rules().next().unwrap().evaluate(), expected);
    }
}

#[test]
fn recursive_union_binding_has_linear_path_work_for_every_container() {
    // Leave room for the ordinary validator's recursive container frames;
    // the budget measures copied path segments independently of stack size.
    std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn(|| {
    const COUNT: usize = 24;
    for bool_first in [false, true] {
        for container in ["field", "list", "map"] {
            let child = match container {
                "field" => AttributeType::ref_("Node"),
                "list" => AttributeType::list(AttributeType::ref_("Node")),
                "map" => AttributeType::map(AttributeType::ref_("Node")),
                _ => unreachable!(),
            };
            let object_type = AttributeType::struct_with_rules(
                "NodeObject",
                vec![
                    StructField::optional("target", AttributeType::bool()).build(),
                    StructField::optional("child", child).build(),
                ],
                vec![],
                vec![InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("node.target").unwrap(),
                    InputPredicate::Present,
                )],
            );
            let members = if bool_first {
                vec![AttributeType::bool(), object_type]
            } else {
                vec![object_type, AttributeType::bool()]
            };
            let schemas = registry(
                ResourceSchema::new("Review")
                    .with_def("Node", AttributeType::union(members))
                    .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build()),
            );
            let mut value = object([("target", Value::Concrete(ConcreteValue::Bool(true)))]);
            for _ in 1..COUNT {
                let child = match container {
                    "field" => value,
                    "list" => Value::Concrete(ConcreteValue::List(vec![value])),
                    "map" => object([("entry", value)]),
                    _ => unreachable!(),
                };
                value = object([("target", Value::Concrete(ConcreteValue::Bool(true))), ("child", child)]);
            }
            let resource = Resource::with_provider("mock", "Review", "example", None)
                .with_attribute("node", value);
            let site = classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                .unwrap().unwrap();
            let (_, segments) = crate::schema::with_field_path_work(|| {
                let results = site.applicable_rules().map(|rule| rule.evaluate()).collect::<Vec<_>>();
                assert_eq!(results, vec![InputVerdict::Proven; COUNT]);
            });
            assert!(segments <= COUNT * 16,
                "{container}, bool_first={bool_first}: {segments} path segments for {COUNT} satisfied nodes");
        }
    }
    }).unwrap().join().unwrap();
}

#[test]
fn deep_ignore_masks_have_linear_prefix_work() {
    const COUNT: usize = 32;
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::struct_with_rules(
                    "Node",
                    vec![
                        StructField::optional("target", AttributeType::string()).build(),
                        StructField::optional("child", AttributeType::ref_("Node")).build(),
                    ],
                    vec![],
                    vec![InputRule::value(
                        InputRuleScope::EveryWrite,
                        InputPath::new("target").unwrap(),
                        InputRuleCode::new("node.target").unwrap(),
                        InputPredicate::Present,
                    )],
                ),
            )
            .attribute(AttributeSchema::optional("root", AttributeType::ref_("Node")).build()),
    );
    let mut value = object([("target", text("valid"))]);
    for _ in 1..COUNT {
        value = object([("target", text("valid")), ("child", value)]);
    }
    let previous = value.clone();
    let mut desired =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("root", value);
    let mut path = vec![ParsedIgnorePathSegment::Attribute("root".into())];
    path.extend((1..COUNT).map(|_| ParsedIgnorePathSegment::Attribute("child".into())));
    path.push(ParsedIgnorePathSegment::Attribute("target".into()));
    desired.directives.ignore_changes = vec![ParsedIgnorePath::new(path)];
    let finalized = update(desired, previous, "root", &schemas);
    let (_, comparisons) = rule_input::with_ignore_path_work(|| {
        finalized.check_inputs(&schemas, &HashMap::new()).unwrap();
    });
    assert!(
        comparisons <= COUNT * 16,
        "{comparisons} ignore-prefix comparisons for {COUNT} recursive instances"
    );
}

#[test]
fn wide_ignore_masks_have_linear_prefix_work() {
    const COUNT: usize = 128;
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                InputRuleCode::new("tags.reserved").unwrap(),
                InputPredicate::NoPrefix(crate::schema::NonEmptyString::new("aws:").unwrap()),
            )),
    );
    let value = Value::Concrete(ConcreteValue::Map(
        (0..COUNT)
            .map(|index| (format!("key_{index}"), text("valid")))
            .collect(),
    ));
    let previous = value.clone();
    let mut desired =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("tags", value);
    desired.directives.ignore_changes = (0..COUNT)
        .map(|index| {
            ParsedIgnorePath::new(vec![
                ParsedIgnorePathSegment::Attribute("tags".into()),
                ParsedIgnorePathSegment::MapKey(format!("key_{index}")),
            ])
        })
        .collect();
    let finalized = update(desired, previous, "tags", &schemas);
    let (_, comparisons) = rule_input::with_ignore_path_work(|| {
        finalized.check_inputs(&schemas, &HashMap::new()).unwrap();
    });
    assert!(
        comparisons <= COUNT * 16,
        "{comparisons} ignore-prefix comparisons for {COUNT} map entries"
    );
}
