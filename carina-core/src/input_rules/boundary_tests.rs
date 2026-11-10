use super::*;
use crate::resource::Resource;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

fn alias_schema() -> SchemaRegistry {
    let entry = AttributeType::struct_with_rules(
        "Entry",
        vec![
            StructField::optional(
                "rules",
                AttributeType::list(AttributeType::struct_("Rule", vec![])),
            )
            .with_block_name("rule")
            .build(),
            StructField::optional("target", AttributeType::bool()).build(),
            StructField::optional("description", AttributeType::string()).build(),
        ],
        vec![],
        vec![
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("entry.target").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Present(InputPath::new("rules").unwrap())),
        ],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("entries", AttributeType::map(entry)).build())
            .validate()
            .unwrap(),
    );
    schemas
}

fn check_map_aliases(protected: bool) {
    let schemas = alias_schema();
    for spelling in ["rules", "rule"] {
        let entry = object([(
            spelling,
            Value::Concrete(ConcreteValue::List(vec![object([])])),
        )]);
        for nested_protection in [false, true] {
            let entry = if protected && nested_protection {
                entry.clone().into_secret()
            } else {
                entry.clone()
            };
            let entries = object([("private-key", entry)]);
            let entries = if protected && !nested_protection {
                entries.into_secret()
            } else {
                entries
            };
            let resource = Resource::with_provider("mock", "Review", "example", None)
                .with_attribute("entries", entries);
            let inputs = AuthoredInputs::resource_for_test(&resource);
            let report = check_authored_inputs(&inputs, &schemas);
            assert_eq!(
                report.findings().len(),
                1,
                "{spelling}, protected={protected}, nested={nested_protection}: {report:?}"
            );
            assert!(report.pending().is_empty());
            let finding = &report.findings()[0];
            assert_eq!(
                finding.violation().path().as_str(),
                if protected && !nested_protection {
                    "entries[\"<secret>\"].target"
                } else {
                    "entries[\"private-key\"].target"
                }
            );
            if protected && !nested_protection {
                assert!(!format!("{report:?}").contains("private-key"));
            }
        }
    }
}

#[test]
fn authored_map_block_aliases_are_canonical_before_rule_evaluation() {
    check_map_aliases(false);
}

#[test]
fn authored_secret_map_block_aliases_preserve_protection_and_rules() {
    check_map_aliases(true);
}

#[test]
fn canonical_root_aliases_precede_default_tag_membership() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .with_block_name("tag")
                    .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("tagged.target").unwrap(),
                    InputPredicate::Present,
                )
                .with_condition(InputCondition::Present(InputPath::new("tags").unwrap())),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file("provider mock { default_tags = { owner = 'team' } }\nlet example = mock.Review { tag = { explicit = 'yes' } }\n",
        std::path::Path::new("main.crn"), &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())).unwrap();
    let report = check_authored_inputs(parsed.authored_inputs().unwrap(), &schemas);
    assert!(
        report.preparation_errors().is_empty(),
        "default membership must not manufacture an alias conflict: {report:?}"
    );
    assert_eq!(report.findings().len(), 1);
    assert!(report.pending().is_empty());
}

#[test]
fn ignored_alias_guards_are_withheld_after_canonical_binding() {
    use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment as Segment};
    let schemas = alias_schema();
    let mut resource = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "entries",
        object([(
            "one",
            object([
                (
                    "rule",
                    Value::Concrete(ConcreteValue::List(vec![object([])])),
                ),
                (
                    "description",
                    Value::Concrete(ConcreteValue::String("new".into())),
                ),
            ]),
        )]),
    );
    resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        Segment::Attribute("entries".into()),
        Segment::MapKey("one".into()),
        Segment::Attribute("rules".into()),
    ])];
    let current = crate::resource::SnapshotState::existing(
        resource.id.clone(),
        crate::test_support::snapshot_attributes(HashMap::from([(
            "entries".into(),
            object([(
                "one",
                object([
                    (
                        "rules",
                        Value::Concrete(ConcreteValue::List(vec![object([])])),
                    ),
                    (
                        "description",
                        Value::Concrete(ConcreteValue::String("old".into())),
                    ),
                ]),
            )]),
        )])),
    )
    .with_identifier("existing");
    let desired =
        crate::resource::ResolvedResource::resolve_raw_fixture(resource, &schemas).unwrap();
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(current),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to: desired,
        changed_attributes: vec!["entries".into()],
    });
    let checked = FinalizedPlan::new(plan)
        .check_inputs(&schemas, &HashMap::new())
        .expect("ignored canonical guard cannot use its unapplied alias spelling");
    assert_eq!(checked.pending_rows(0).len(), 1);
}

#[test]
fn canonical_recursive_authored_construction_has_linear_path_work() {
    const DEPTH: usize = 32;
    let local = AttributeType::struct_with_rules(
        "Node",
        vec![
            StructField::optional("target", AttributeType::bool()).build(),
            StructField::optional("child", AttributeType::ref_("Node")).build(),
            StructField::optional(
                "rules",
                AttributeType::list(AttributeType::struct_("Rule", vec![])),
            )
            .with_block_name("rule")
            .build(),
        ],
        vec![],
        vec![InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("node.target").unwrap(),
            InputPredicate::Equals(InputLiteral::Bool(true)),
        )],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .with_def("Node", local)
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build())
            .validate()
            .unwrap(),
    );
    let mut value = object([("target", Value::Concrete(ConcreteValue::Bool(true)))]);
    for _ in 1..DEPTH {
        value = object([
            ("target", Value::Concrete(ConcreteValue::Bool(true))),
            ("child", value),
        ]);
    }
    let resource =
        Resource::with_provider("mock", "Review", "example", None).with_attribute("node", value);
    let inputs = AuthoredInputs::resource_for_test(&resource);
    let (_, work) = crate::schema::with_block_name_path_work(|| {
        let report = check_authored_inputs(&inputs, &schemas);
        assert!(
            report.findings().is_empty() && report.pending().is_empty(),
            "{report:?}"
        );
    });
    assert!(
        work <= DEPTH * 64,
        "{work} path work for {DEPTH} canonical authored nodes"
    );
    let mut resources = vec![resource];
    let (_, work) = crate::schema::with_block_name_path_work(|| {
        assert!(crate::schema::resolve_block_names_detailed(&mut resources, &schemas).is_empty());
    });
    assert!(
        work <= DEPTH * 64,
        "{work} shared canonicalizer path work for {DEPTH} canonical nodes"
    );
}

#[test]
fn unavailable_provider_union_authority_retains_a_pending_obligation() {
    let arm = |name: &str, scalar: &str, rules| {
        AttributeType::struct_with_rules(
            name,
            vec![
                StructField::required(
                    "kind",
                    AttributeType::refined_string(
                        Some(crate::schema::TypeIdentity::bare(scalar)),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
                StructField::optional("target", AttributeType::bool()).build(),
            ],
            vec![],
            rules,
        )
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional(
                    "node",
                    AttributeType::union(vec![
                        arm("A", "First", vec![]),
                        arm(
                            "B",
                            "Second",
                            vec![InputRule::value(
                                InputRuleScope::EveryWrite,
                                InputPath::new("target").unwrap(),
                                InputRuleCode::new("union.target").unwrap(),
                                InputPredicate::Present,
                            )],
                        ),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let resource = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "node",
        object([("kind", Value::Concrete(ConcreteValue::String("b".into())))]),
    );
    let report = check_authored_inputs(&AuthoredInputs::resource_for_test(&resource), &schemas);
    assert!(report.findings().is_empty());
    assert_eq!(
        report.pending().len(),
        1,
        "missing callback authority is pending, never a declaration-order pass"
    );
}

#[test]
fn recursive_provider_union_admission_reuses_fits_with_a_constructor_budget() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            const DEPTH: usize = 32;
            let kind = AttributeType::refined_string(
                Some(crate::schema::TypeIdentity::bare("Second")),
                None,
                None,
                None,
            );
            let node = AttributeType::struct_with_rules(
                "Node",
                vec![
                    StructField::required("kind", kind).build(),
                    StructField::optional("target", AttributeType::bool()).build(),
                    StructField::optional("child", AttributeType::ref_("Node")).build(),
                ],
                vec![],
                vec![InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("target").unwrap(),
                    InputRuleCode::new("node.target").unwrap(),
                    InputPredicate::Equals(InputLiteral::Bool(true)),
                )],
            );
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                ResourceSchema::new("Review")
                    .with_def(
                        "Node",
                        AttributeType::union(vec![node, AttributeType::bool()]),
                    )
                    .attribute(
                        AttributeSchema::optional("node", AttributeType::ref_("Node")).build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let leaf = || {
                object([
                    (
                        "kind",
                        Value::Concrete(ConcreteValue::String("accepted".into())),
                    ),
                    ("target", Value::Concrete(ConcreteValue::Bool(true))),
                ])
            };
            let mut value = leaf();
            for _ in 1..DEPTH {
                let mut fields = match leaf() {
                    Value::Concrete(ConcreteValue::Map(fields)) => fields,
                    _ => unreachable!(),
                };
                fields.insert("child".into(), value);
                value = Value::Concrete(ConcreteValue::Map(fields));
            }
            let resource = Resource::with_provider("mock", "Review", "example", None)
                .with_attribute("node", value);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let mut catalog =
                crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
            catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
                HashSet::new(),
                Box::new(move |_, _| {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }),
            );
            crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
                let inputs =
                    AuthoredInputs::resource_for_test(&resource).resolved_in_session(session);
                let (_, paths) = crate::schema::with_field_path_work(|| {
                    let report = check_authored_inputs(&inputs, &schemas);
                    assert!(
                        report.findings().is_empty() && report.pending().is_empty(),
                        "{report:?}"
                    );
                });
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    DEPTH,
                    "one callback per evidence node, including construction"
                );
                assert!(
                    paths <= DEPTH * 48,
                    "{paths} path work for {DEPTH} admitted recursive union nodes"
                );
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn check_shared_alias_walker(protected: bool) {
    let entry = AttributeType::struct_(
        "Entry",
        vec![
            StructField::optional(
                "rules",
                AttributeType::list(AttributeType::struct_("Rule", vec![])),
            )
            .with_block_name("rule")
            .build(),
        ],
    );
    for container in ["struct", "list", "map", "union", "nested"] {
        let value = object([(
            "rule",
            Value::Concrete(ConcreteValue::List(vec![object([])])),
        )]);
        let (ty, value) = match container {
            "struct" => (entry.clone(), value),
            "list" => (
                AttributeType::list(entry.clone()),
                Value::Concrete(ConcreteValue::List(vec![value])),
            ),
            "map" => (
                AttributeType::map(entry.clone()),
                object([("private-key", value)]),
            ),
            "union" => (
                AttributeType::union(vec![entry.clone(), AttributeType::bool()]),
                value,
            ),
            "nested" => (
                AttributeType::list(AttributeType::map(AttributeType::union(vec![
                    entry.clone(),
                    AttributeType::bool(),
                ]))),
                Value::Concrete(ConcreteValue::List(vec![object([("private-key", value)])])),
            ),
            _ => unreachable!(),
        };
        let value = if protected {
            value.into_secret()
        } else {
            value
        };
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Review")
                .attribute(AttributeSchema::optional("objects", ty).build())
                .validate()
                .unwrap(),
        );
        let mut resource = Resource::with_provider("mock", "Review", "example", None)
            .with_attribute("objects", value);
        let schema = schemas.get_for(&resource).unwrap().schema();
        let before = crate::schema::find_resource_block_name_alias(&resource, schema);
        assert!(
            before.is_some(),
            "{container}, protected={protected}: read-only alias checking skipped an instance"
        );
        if protected {
            assert!(!format!("{before:?}").contains("private-key"));
        }
        let conflicts = crate::schema::resolve_block_names_detailed(
            std::slice::from_mut(&mut resource),
            &schemas,
        );
        assert!(conflicts.is_empty(), "{conflicts:?}");
        let after = crate::schema::find_resource_block_name_alias(&resource, schema);
        assert!(
            after.is_none(),
            "{container}, protected={protected}: {after:?}"
        );
        fn contains_alias(value: &Value) -> bool {
            match value {
                Value::Concrete(ConcreteValue::Map(map)) => {
                    map.contains_key("rule") || map.values().any(contains_alias)
                }
                Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_alias),
                Value::Deferred(DeferredValue::Secret(payload)) => {
                    contains_alias(payload.plaintext_for_test())
                }
                _ => false,
            }
        }
        assert!(
            !contains_alias(&resource.attributes["objects"]),
            "{container}, protected={protected}: normalization skipped an instance"
        );
    }
}

#[test]
fn shared_alias_walker_covers_maps_unions_and_nested_collections() {
    check_shared_alias_walker(false);
}

#[test]
fn shared_alias_walker_covers_secret_wrappers_without_disclosing_keys() {
    check_shared_alias_walker(true);
}
