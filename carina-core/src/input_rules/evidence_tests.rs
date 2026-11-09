use super::*;
use crate::{
    builtins::BuiltinFunction,
    resource::{DiagnosticSourceLine, Resource, UnavailableReason},
    schema::{
        AttributeSchema, AttributeType, InputViolationReason, ResourceSchema, StructField,
        TypeIdentity,
    },
};

fn path(name: &str) -> InputPath {
    InputPath::new(name).unwrap()
}

fn rule(
    target: &str,
    predicate: InputPredicate<crate::schema::PatternSpec>,
    condition: InputCondition,
) -> InputRule {
    InputRule::value(
        InputRuleScope::EveryWrite,
        path(target),
        InputRuleCode::new("review.rule").unwrap(),
        predicate,
    )
    .with_condition(condition)
}

fn registry(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    schemas
}

fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

fn pending() -> Value {
    Value::Deferred(DeferredValue::Unknown(
        crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::not_found(),
        ),
    ))
}

fn evaluate(
    schemas: &SchemaRegistry,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> Vec<InputVerdict> {
    let mut resource = Resource::with_provider("mock", "Review", "example", None);
    resource.attributes = fields
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
    let site = classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), schemas)
        .unwrap()
        .unwrap();
    site.applicable_rules()
        .map(|rule| rule.evaluate())
        .collect()
}

fn assert_failed(verdict: &InputVerdict) {
    assert!(
        matches!(verdict, InputVerdict::Violation(v) if v.reason() == InputViolationReason::EvaluationFailed),
        "{verdict:?}"
    );
}

#[test]
fn unsent_nested_omissions_remain_withheld_at_plan_and_dispatch() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![StructField::optional("enabled", AttributeType::bool()).build()],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule(
                "target",
                InputPredicate::Equals(InputLiteral::String("allowed".into())),
                InputCondition::Equals {
                    path: path("settings.enabled"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    let desired = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("settings", object([]))
        .with_attribute(
            "target",
            Value::Concrete(ConcreteValue::String("bad".into())),
        );
    let previous = crate::resource::State::existing(
        desired.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [
                (
                    "settings".into(),
                    object([("enabled", Value::Concrete(ConcreteValue::Bool(true)))]),
                ),
                (
                    "target".into(),
                    Value::Concrete(ConcreteValue::String("allowed".into())),
                ),
            ],
        )),
    )
    .with_identifier("existing");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to: resolved_input_fixture(desired.clone(), &schemas),
        changed_attributes: vec!["target".into()],
    });
    let finalized = FinalizedPlan::new(plan);
    let current = HashMap::new();
    let checked = finalized.check_inputs(&schemas, &current).unwrap();
    assert_eq!(
        checked.pending_rows(0).len(),
        1,
        "an unsent child omission cannot disable its guard"
    );
    let patch = crate::provider::UpdatePatch {
        ops: vec![crate::provider::PatchOp::Replace {
            key: "target".into(),
            value: Value::Concrete(ConcreteValue::String("bad".into())),
        }],
    };
    let mut site =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&desired), &schemas)
            .unwrap()
            .unwrap();
    site.context = InputWriteContext::Update;
    let mut update = UpdateInputContext {
        desired: project_attributes(&desired.attributes),
        changed: HashSet::new(),
        removed: HashSet::new(),
        ignored: Default::default(),
    };
    site.attributes = Some(Cow::Owned(update.overlay(&patch)));
    site.update = Some(update);
    assert_eq!(
        site.applicable_rules().next().unwrap().evaluate(),
        InputVerdict::Undecided,
        "dispatch must use the same unsent-descendant boundary"
    );
    for op in [
        crate::provider::PatchOp::Replace {
            key: "settings".into(),
            value: object([]),
        },
        crate::provider::PatchOp::Remove {
            key: "settings".into(),
        },
    ] {
        let patch = crate::provider::UpdatePatch {
            ops: vec![
                crate::provider::PatchOp::Replace {
                    key: "target".into(),
                    value: Value::Concrete(ConcreteValue::String("bad".into())),
                },
                op,
            ],
        };
        let update = site.update.as_mut().unwrap();
        site.attributes = Some(Cow::Owned(update.overlay(&patch)));
        assert_eq!(
            site.applicable_rules().next().unwrap().evaluate(),
            InputVerdict::Proven,
            "an exact patch omission or removal supplies absence evidence"
        );
    }
}

#[test]
fn false_conditions_skip_pending_and_failed_struct_instances() {
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::struct_with_rules(
                    "Settings",
                    vec![StructField::optional("target", AttributeType::string()).build()],
                    vec![],
                    vec![rule(
                        "target",
                        InputPredicate::Present,
                        InputCondition::Not(Box::new(InputCondition::Always)),
                    )],
                ),
            )
            .build(),
        ),
    );
    for value in [pending(), failed_value()] {
        assert_eq!(
            evaluate(&schemas, [("settings", value)]),
            vec![InputVerdict::Proven]
        );
    }
}

#[test]
fn unknown_reused_definitions_produce_one_deferred_subtree_obligation() {
    const DEPTH: usize = 12;
    let mut schema = ResourceSchema::new("Review").with_def(
        "Node12",
        AttributeType::struct_with_rules(
            "Leaf",
            vec![StructField::optional("target", AttributeType::string()).build()],
            vec![],
            vec![rule(
                "target",
                InputPredicate::Present,
                InputCondition::Always,
            )],
        ),
    );
    for depth in (0..DEPTH).rev() {
        schema = schema.with_def(
            format!("Node{depth}"),
            AttributeType::struct_(
                "Branch",
                vec![
                    StructField::optional(
                        "left",
                        AttributeType::ref_(format!("Node{}", depth + 1)),
                    )
                    .build(),
                    StructField::optional(
                        "right",
                        AttributeType::ref_(format!("Node{}", depth + 1)),
                    )
                    .build(),
                ],
            ),
        );
    }
    let schemas = registry(
        schema
            .clone()
            .attribute(AttributeSchema::optional("root", AttributeType::ref_("Node0")).build()),
    );
    let verdicts = evaluate(&schemas, [("root", pending())]);
    assert_eq!(
        verdicts.len(),
        1,
        "unknown child membership must not expand the declaration DAG"
    );
    assert_eq!(verdicts[0], InputVerdict::Undecided);
    let lists = registry(
        schema.attribute(
            AttributeSchema::optional("root", AttributeType::list(AttributeType::ref_("Node0")))
                .build(),
        ),
    );
    let verdicts = evaluate(
        &lists,
        [(
            "root",
            Value::Concrete(ConcreteValue::List(vec![pending(); 1_000])),
        )],
    );
    assert_eq!(
        verdicts.len(),
        1_000,
        "one obligation per existing unknown member"
    );
    assert!(verdicts.iter().all(|v| *v == InputVerdict::Undecided));
    let mut branch = object([(
        "target",
        Value::Concrete(ConcreteValue::String("valid".into())),
    )]);
    for _ in 1..DEPTH {
        branch = object([("left", branch)]);
    }
    assert_eq!(
        evaluate(
            &schemas,
            [(
                "root",
                object([("left", branch.clone()), ("right", branch)])
            )]
        ),
        vec![InputVerdict::Proven; 2],
        "known membership instantiates each concrete descendant again"
    );
}

#[test]
fn recursive_rule_paths_have_quadratic_total_segment_work() {
    fn work(depth: usize, update: bool) -> usize {
        let schemas = registry(
            ResourceSchema::new("Review")
                .with_def(
                    "Node",
                    AttributeType::struct_with_rules(
                        "Node",
                        vec![
                            StructField::optional("guard", AttributeType::bool()).build(),
                            StructField::optional("target", AttributeType::string()).build(),
                            StructField::optional("child", AttributeType::ref_("Node")).build(),
                        ],
                        vec![],
                        vec![rule(
                            "target",
                            InputPredicate::Present,
                            InputCondition::Present(path("guard")),
                        )],
                    ),
                )
                .attribute(AttributeSchema::optional("root", AttributeType::ref_("Node")).build()),
        );
        let mut value = object([
            ("guard", Value::Concrete(ConcreteValue::Bool(true))),
            (
                "target",
                Value::Concrete(ConcreteValue::String("valid".into())),
            ),
        ]);
        for _ in 1..depth {
            value = object([
                ("guard", Value::Concrete(ConcreteValue::Bool(true))),
                (
                    "target",
                    Value::Concrete(ConcreteValue::String("valid".into())),
                ),
                ("child", value),
            ]);
        }
        let resource = Resource::with_provider("mock", "Review", "example", None)
            .with_attribute("root", value);
        let mut site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                .unwrap()
                .unwrap();
        if update {
            site.context = InputWriteContext::Update;
            site.update = Some(UpdateInputContext {
                desired: project_attributes(&resource.attributes),
                changed: HashSet::from(["root".into()]),
                removed: HashSet::new(),
                ignored: Default::default(),
            });
        }
        let (_, segments) = crate::schema::with_field_path_work(|| {
            let verdicts = site
                .applicable_rules()
                .map(|rule| rule.evaluate())
                .collect::<Vec<_>>();
            assert_eq!(verdicts, vec![InputVerdict::Proven; depth]);
        });
        segments
    }
    for update in [false, true] {
        let small = work(16, update);
        let large = work(32, update);
        assert!(
            large <= small * 5,
            "update={update}: doubling depth cost {small} -> {large} path segments; expected at most quadratic work"
        );
    }
}

#[test]
fn recursive_union_does_not_revisit_same_value() {
    let node = AttributeType::struct_with_rules(
        "NodeObject",
        vec![
            StructField::optional("x", AttributeType::bool()).build(),
            StructField::optional("child", AttributeType::ref_("Node")).build(),
        ],
        vec![],
        vec![rule("x", InputPredicate::Present, InputCondition::Always)],
    );
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::union(vec![node, AttributeType::ref_("Node")]),
            )
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build()),
    );
    let verdicts = evaluate(
        &schemas,
        [(
            "node",
            object([
                ("x", Value::Concrete(ConcreteValue::Bool(true))),
                ("child", object([])),
            ]),
        )],
    );
    assert_eq!(verdicts.len(), 2);
    assert!(verdicts.contains(&InputVerdict::Proven));
    assert!(
        verdicts.iter().any(
            |v| matches!(v, InputVerdict::Violation(v) if v.path().as_str() == "node.child.x")
        )
    );
}

#[test]
fn union_rules_follow_selected_member() {
    let fields = || vec![StructField::optional("x", AttributeType::bool()).build()];
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "node",
                AttributeType::union(vec![
                    AttributeType::struct_("A", fields()),
                    AttributeType::struct_with_rules(
                        "B",
                        fields(),
                        vec![],
                        vec![rule("x", InputPredicate::Present, InputCondition::Always)],
                    ),
                ]),
            )
            .build(),
        ),
    );
    assert!(
        evaluate(&schemas, [("node", object([]))]).is_empty(),
        "unselected B must not supply a rule"
    );
    assert_eq!(
        evaluate(&schemas, [("node", pending())]),
        vec![InputVerdict::Undecided]
    );
    let fields = |tag| {
        vec![
            StructField::required(
                "tag",
                AttributeType::refined_string(
                    None,
                    Some(crate::schema::PatternSpec::new(
                        tag,
                        crate::schema::PatternMatch::Full,
                    )),
                    None,
                    None,
                ),
            )
            .build(),
            StructField::optional("x", AttributeType::bool()).build(),
        ]
    };
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "node",
                AttributeType::union(vec![
                    AttributeType::struct_("A", fields("a")),
                    AttributeType::struct_with_rules(
                        "B",
                        fields("b"),
                        vec![],
                        vec![rule("x", InputPredicate::Present, InputCondition::Always)],
                    ),
                ]),
            )
            .build(),
        ),
    );
    assert_eq!(
        evaluate(&schemas, [("node", object([("tag", pending())]))]),
        vec![InputVerdict::Undecided]
    );
}

#[test]
fn float_equality_accepts_exact_integer_representations() {
    for (integer, float, equal) in [
        (1, 1.0, true),
        (0, -0.0, true),
        (i64::MIN, -9_223_372_036_854_775_808.0, true),
        (9_007_199_254_740_993, 9_007_199_254_740_992.0, false),
        (i64::MAX, 9_223_372_036_854_775_808.0, false),
    ] {
        let literal = InputLiteral::Float(crate::FiniteF64::new(float).unwrap());
        let base = || {
            ResourceSchema::new("Review")
                .attribute(AttributeSchema::optional("guard", AttributeType::float()).build())
                .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
        };
        let schemas = registry(base().input_rule(rule(
            "target",
            InputPredicate::Present,
            InputCondition::Equals {
                path: path("guard"),
                literal: literal.clone(),
            },
        )));
        let verdicts = evaluate(
            &schemas,
            [("guard", Value::Concrete(ConcreteValue::Int(integer)))],
        );
        assert_eq!(
            matches!(&verdicts[0], InputVerdict::Violation(v) if v.reason() == InputViolationReason::Present),
            equal,
            "guard: {integer} compared to {float}: {verdicts:?}"
        );
        let schemas = registry(base().input_rule(rule(
            "guard",
            InputPredicate::Equals(literal),
            InputCondition::Always,
        )));
        let verdicts = evaluate(
            &schemas,
            [("guard", Value::Concrete(ConcreteValue::Int(integer)))],
        );
        assert_eq!(
            verdicts[0] == InputVerdict::Proven,
            equal,
            "target: {integer} compared to {float}: {verdicts:?}"
        );
    }
}

fn failed_coalesce<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>()
-> Value<P, B> {
    Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::Deferred(DeferredValue::Unavailable {
            name: BuiltinFunction::env,
            args: vec![],
            reason: UnavailableReason::from_builtin_error(
                "private nested attempted failure".into(),
            ),
            source_line: DiagnosticSourceLine::default(),
        })),
        default: Box::new(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "flag".into(),
            Value::Concrete(ConcreteValue::Bool(true)),
        )])))),
    })
}

#[test]
fn nested_guard_retains_coalesce_failure() {
    for condition in [
        InputCondition::Present(path("settings.flag")),
        InputCondition::Not(Box::new(InputCondition::Present(path("settings.flag")))),
        InputCondition::Not(Box::new(InputCondition::Equals {
            path: path("settings.flag"),
            literal: InputLiteral::Bool(true),
        })),
    ] {
        let schemas = registry(
            ResourceSchema::new("Review")
                .attribute(
                    AttributeSchema::optional(
                        "settings",
                        AttributeType::struct_(
                            "Settings",
                            vec![StructField::optional("flag", AttributeType::bool()).build()],
                        ),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
                .input_rule(rule("target", InputPredicate::Present, condition)),
        );
        let value = failed_coalesce();
        let proof = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap();
        assert!(proof.failure().is_some());
        assert_failed(&evaluate(&schemas, [("settings", value)])[0]);
        let mut file = crate::parser::parse_file(
            "let example = mock.Review { settings = { flag = true } }",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        file.resources[0]
            .attributes
            .insert("settings".into(), failed_coalesce());
        let report = check_authored_inputs(&file, &schemas).unwrap();
        assert_eq!(report.len(), 1);
        assert!(report.pending().is_empty());
        assert_eq!(
            report[0].violation().reason(),
            InputViolationReason::EvaluationFailed
        );
        assert!(
            !report[0]
                .to_string()
                .contains("private nested attempted failure")
        );
    }
}

#[test]
fn known_invalid_enum_is_failed_instead_of_pending() {
    let identity = TypeIdentity::new(Some("mock"), ["Review"], "Namespace");
    let literal = InputLiteral::Enum {
        identity: identity.clone(),
        api_value: "account-regional".into(),
    };
    let enum_type = AttributeType::enum_(
        identity,
        Some(vec!["global".into(), "account-regional".into()]),
        vec![("account-regional".into(), "account_regional".into())],
        None,
        None,
    );
    for target_equality in [false, true] {
        let input_rule = if target_equality {
            rule(
                "guard",
                InputPredicate::Equals(literal.clone()),
                InputCondition::Always,
            )
        } else {
            rule(
                "target",
                InputPredicate::Present,
                InputCondition::Equals {
                    path: path("guard"),
                    literal: literal.clone(),
                },
            )
        };
        let schemas = registry(
            ResourceSchema::new("Review")
                .attribute(AttributeSchema::optional("guard", enum_type.clone()).build())
                .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
                .input_rule(input_rule),
        );
        for value in [
            Value::Concrete(ConcreteValue::String("other-private-value".into())),
            Value::Concrete(ConcreteValue::EnumIdentifier(
                crate::resource::RawEnumIdentifier::parse(
                    "mock.Review.Namespace.other-private-value",
                ),
            )),
        ] {
            let invalid = evaluate(&schemas, [("guard", value)]);
            assert_failed(&invalid[0]);
            assert!(!format!("{invalid:?}").contains("other-private-value"));
        }
        for value in [
            Value::Concrete(ConcreteValue::String("account-regional".into())),
            Value::Concrete(ConcreteValue::String("account_regional".into())),
            Value::Concrete(ConcreteValue::EnumIdentifier(
                crate::resource::RawEnumIdentifier::parse("mock.Review.Namespace.account_regional"),
            )),
        ] {
            let verdicts = evaluate(&schemas, [("guard", value)]);
            if target_equality {
                assert_eq!(verdicts, vec![InputVerdict::Proven]);
            } else {
                assert!(
                    matches!(&verdicts[0], InputVerdict::Violation(v) if v.reason() == InputViolationReason::Present)
                );
            }
        }
        assert_eq!(
            evaluate(&schemas, [("guard", pending())]),
            vec![InputVerdict::Undecided]
        );
    }
}

#[test]
fn deferred_template_retains_literal_guard_evidence() {
    use crate::parser::{DeferredForExpression, DeferredForTarget, ForBinding};
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(rule(
                "name",
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new("-an").unwrap()),
                InputCondition::Equals {
                    path: path("enabled"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    for (enabled, expected) in [
        (Value::Concrete(ConcreteValue::Bool(true)), "violation"),
        (Value::Concrete(ConcreteValue::Bool(false)), "proven"),
        (
            Value::Deferred(DeferredValue::Placeholder(crate::resource::LoopSlot::Value)),
            "pending",
        ),
    ] {
        for replacement in [false, true] {
            let template: DeferredForExpression = DeferredForExpression::new(
                None,
                1,
                "for item in parent.items".into(),
                DeferredForTarget::bound(
                    crate::resource::RootScope::new()
                        .resolve(crate::resource::RelativeBindingName::new("children")),
                )
                .unwrap(),
                Value::root_resource_ref("parent", "items", vec![]),
                ForBinding::Simple("item".into()),
                Resource::with_provider("mock", "Review", "children", None)
                    .with_attribute("enabled", enabled.clone())
                    .with_attribute(
                        "name",
                        Value::Concrete(ConcreteValue::String("bucket-an".into())),
                    ),
            );
            let mut plan = Plan::new();
            if replacement {
                let old = crate::resource::ResolvedResource::resolve_raw_fixture(
                    Resource::with_provider("mock", "Review", "children", None),
                    &schemas,
                )
                .unwrap();
                let delete = crate::effect::DeferredReplaceDelete {
                    id: old.resolved_id(),
                    identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
                    generation: crate::effect::EffectGeneration::Current,
                    previous_attributes: HashMap::new(),
                    directives: Default::default(),
                    binding: None,
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                };
                plan.add(Effect::DeferredReplace(Box::new(
                    crate::test_support::compiled_replace(
                        crate::effect::NonEmptyDeletes::try_new(vec![delete]).unwrap(),
                        Box::new(template),
                    ),
                )));
            } else {
                plan.add(Effect::DeferredCreate(crate::test_support::compiled_loop(
                    Box::new(template),
                )));
            }
            let finalized = FinalizedPlan::new(plan);
            let current = HashMap::new();
            let site = finalized
                .write_sites(&schemas, &current)
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(
                site.context(),
                if replacement {
                    InputWriteContext::DeferredReplacement
                } else {
                    InputWriteContext::DeferredCreate
                }
            );
            let verdict = site.applicable_rules().next().unwrap().evaluate();
            match expected {
                "violation" => assert!(
                    matches!(verdict, InputVerdict::Violation(ref v) if v.reason() == InputViolationReason::NoSuffix),
                    "{verdict:?}"
                ),
                "proven" => assert_eq!(verdict, InputVerdict::Proven),
                "pending" => assert_eq!(verdict, InputVerdict::Undecided),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn ignored_unsent_guard_does_not_use_unapplied_authored_evidence() {
    use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment, ResolvedIgnoreChanges};
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule(
                "target",
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
                InputCondition::Equals {
                    path: path("guard"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    let resource = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(false)))
        .with_attribute(
            "target",
            Value::Concrete(ConcreteValue::String("bad.fifo".into())),
        );
    let mut site =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
            .unwrap()
            .unwrap();
    site.context = InputWriteContext::Update;
    let mut update = UpdateInputContext {
        desired: project_attributes(&resource.attributes),
        changed: HashSet::new(),
        removed: HashSet::new(),
        ignored: ResolvedIgnoreChanges::indexed_for_test(
            "guard",
            crate::schema::LeafPath::from_segments([]),
            ParsedIgnorePath::new(vec![ParsedIgnorePathSegment::Attribute("guard".into())]),
        ),
    };
    site.attributes = Some(Cow::Owned(update.overlay(&crate::provider::UpdatePatch {
        ops: vec![crate::provider::PatchOp::Replace {
            key: "target".into(),
            value: resource.attributes["target"].clone(),
        }],
    })));
    site.update = Some(update);
    assert_eq!(
        site.applicable_rules().next().unwrap().evaluate(),
        InputVerdict::Undecided,
        "the ignored guard is not sent, and its authored false value is not evidence"
    );
}

fn failed_value<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>() -> Value<P, B>
{
    Value::Deferred(DeferredValue::Unavailable {
        name: BuiltinFunction::env,
        args: vec![],
        reason: UnavailableReason::from_builtin_error("private attempted failure".into()),
        source_line: DiagnosticSourceLine::default(),
    })
}

#[test]
fn authored_arithmetic_and_access_guards_preserve_failed_knowledge() {
    use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, ValidatedIntegerExpr};
    use crate::resource::{AccessMissPolicy, PathSegment, apply_postfix_access};
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::int()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
            .input_rule(rule(
                "target",
                InputPredicate::Present,
                InputCondition::Not(Box::new(InputCondition::Present(path("guard")))),
            )),
    );
    let arithmetic = Value::Deferred(DeferredValue::IntegerArithmetic(
        ValidatedIntegerExpr::new(
            failed_value(),
            IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            IntegerOperatorSpan::synthetic(),
        )
        .unwrap(),
    ));
    let access = apply_postfix_access(
        failed_value(),
        PathSegment::Field {
            name: "flag".into(),
        },
        0,
        AccessMissPolicy::Error,
    )
    .unwrap();
    let mut findings = Vec::new();
    for value in [
        arithmetic.clone(),
        access.clone(),
        arithmetic.into_secret(),
        access.into_secret(),
    ] {
        assert!(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .unwrap()
                .failure()
                .is_some()
        );
        let mut file = crate::parser::parse_file(
            "let example = mock.Review { guard = 1 }",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        file.resources[0].attributes.insert("guard".into(), value);
        let report = check_authored_inputs(&file, &schemas).unwrap();
        assert!(report.pending().is_empty());
        assert!(!format!("{report:?}").contains("private attempted failure"));
        findings.push(
            report
                .iter()
                .map(|finding| finding.violation().reason())
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(
        findings,
        vec![vec![InputViolationReason::EvaluationFailed]; 4]
    );
}

#[test]
fn failed_string_targets_report_the_same_failure_at_resource_and_struct_scope() {
    let local_rule = || {
        rule(
            "target",
            InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
            InputCondition::Equals {
                path: path("guard"),
                literal: InputLiteral::Bool(true),
            },
        )
    };
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(local_rule())
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_with_rules(
                        "Settings",
                        vec![
                            StructField::optional("guard", AttributeType::bool()).build(),
                            StructField::optional("target", AttributeType::string()).build(),
                        ],
                        vec![],
                        vec![local_rule()],
                    ),
                )
                .build(),
            ),
    );
    for secret in [false, true] {
        let failed = if secret {
            failed_value().into_secret()
        } else {
            failed_value()
        };
        let resource = Resource::with_provider("mock", "Review", "example", None)
            .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(true)))
            .with_attribute("target", failed.clone())
            .with_attribute(
                "settings",
                object([
                    ("guard", Value::Concrete(ConcreteValue::Bool(true))),
                    ("target", failed),
                ]),
            );
        let authored =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                .unwrap()
                .unwrap();
        let verdicts = authored
            .applicable_rules()
            .map(|rule| rule.evaluate())
            .collect::<Vec<_>>();
        assert_eq!(verdicts.len(), 2);
        for verdict in &verdicts {
            assert_failed(verdict);
        }
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            crate::resource::ResolvedResource::resolve_raw_fixture(resource, &schemas).unwrap(),
        ));
        let finalized = FinalizedPlan::new(plan);
        let current = HashMap::new();
        let site = finalized
            .write_sites(&schemas, &current)
            .next()
            .unwrap()
            .unwrap();
        for rule in site.applicable_rules() {
            assert_failed(&rule.evaluate());
        }
    }
}

fn guard_only_update(
    schemas: &SchemaRegistry,
    target: Value,
    ignored_target: bool,
) -> FinalizedPlan {
    let mut resource = Resource::with_provider("mock", "Review", "example", None)
        .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(true)))
        .with_attribute("target", target);
    if ignored_target {
        resource.directives.ignore_changes = vec![crate::resource::ParsedIgnorePath::new(vec![
            crate::resource::ParsedIgnorePathSegment::Attribute("target".into()),
        ])];
    }
    let previous = crate::resource::State::existing(
        resource.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [
                ("guard".into(), Value::Concrete(ConcreteValue::Bool(false))),
                (
                    "target".into(),
                    Value::Concrete(ConcreteValue::String("other".into())),
                ),
            ],
        )),
    )
    .with_identifier("existing");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: crate::provider::AppliedIdentifier::new("existing").unwrap(),
        to: resolved_input_fixture(resource, schemas),
        changed_attributes: vec!["guard".into()],
    });
    FinalizedPlan::new(plan)
}

fn resolved_input_fixture(
    resource: Resource,
    schemas: &SchemaRegistry,
) -> crate::resource::ResolvedResource {
    let mut producer = Resource::with_provider("mock", "Review", "producer", None);
    producer.binding = Some(crate::resource::absolute_binding_address_for_test(
        "producer",
    ));
    let index = crate::resolved_graph::ExpandedBindingIndex::for_test_resources(&[
        producer,
        resource.clone(),
    ])
    .unwrap();
    index.resolve_resource(resource, schemas).unwrap()
}

#[test]
fn ignored_unsent_target_cannot_prove_a_conditional_equality() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule(
                "target",
                InputPredicate::Equals(InputLiteral::String("required".into())),
                InputCondition::Equals {
                    path: path("guard"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    let finalized = guard_only_update(
        &schemas,
        Value::Concrete(ConcreteValue::String("required".into())),
        true,
    );
    let current = HashMap::new();
    let site = finalized
        .write_sites(&schemas, &current)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        site.applicable_rules().next().unwrap().evaluate(),
        InputVerdict::Undecided
    );
    let checked = finalized.check_inputs(&schemas, &current).unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    assert_eq!(checked.pending_rows(0).len(), 1);
}

#[test]
fn pending_unsent_string_target_retains_its_update_obligation_and_row() {
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule(
                "target",
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
                InputCondition::Equals {
                    path: path("guard"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    let finalized = guard_only_update(
        &schemas,
        Value::root_resource_ref("producer", "name", vec![]),
        false,
    );
    let current = HashMap::new();
    let site = finalized
        .write_sites(&schemas, &current)
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        site.applicable_rules().next().unwrap().evaluate(),
        InputVerdict::Undecided
    );
    let checked = finalized.check_inputs(&schemas, &current).unwrap();
    assert_eq!(checked.pending_inputs().len(), 1);
    assert_eq!(checked.pending_rows(0).len(), 1);
}

#[test]
fn conditional_suffix_estimation_is_shared_by_every_object_instance() {
    let local_rule = || {
        rule(
            "name",
            InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
            InputCondition::Equals {
                path: path("guard"),
                literal: InputLiteral::Bool(true),
            },
        )
    };
    let fields = || {
        vec![
            StructField::optional("guard", AttributeType::bool()).build(),
            StructField::optional("name", AttributeType::string()).build(),
        ]
    };
    let mut node_fields = fields();
    node_fields.push(StructField::optional("child", AttributeType::ref_("Node")).build());
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def(
                "Config",
                AttributeType::struct_with_rules("Config", fields(), vec![], vec![local_rule()]),
            )
            .with_def(
                "Node",
                AttributeType::struct_with_rules("Node", node_fields, vec![], vec![local_rule()]),
            )
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("config", AttributeType::ref_("Config")).build())
            .attribute(
                AttributeSchema::optional(
                    "items",
                    AttributeType::list(AttributeType::ref_("Config")),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "by_name",
                    AttributeType::map(AttributeType::ref_("Config")),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build())
            .input_rule(local_rule()),
    );
    for secret in [false, true] {
        let expression = Value::Deferred(DeferredValue::Interpolation(vec![
            crate::resource::InterpolationPart::Expr(Value::root_resource_ref(
                "producer",
                "name",
                vec![],
            )),
            crate::resource::InterpolationPart::Literal(".fifo".into()),
        ]));
        let expression = if secret {
            expression.into_secret()
        } else {
            expression
        };
        let instance = || {
            object([
                ("guard", Value::Concrete(ConcreteValue::Bool(true))),
                ("name", expression.clone()),
            ])
        };
        let fields = [
            ("guard", Value::Concrete(ConcreteValue::Bool(true))),
            ("name", expression.clone()),
            ("config", instance()),
            (
                "items",
                Value::Concrete(ConcreteValue::List(vec![instance()])),
            ),
            ("by_name", object([("entry", instance())])),
            ("node", object([("child", instance())])),
        ];
        let verdicts = evaluate(&schemas, fields.clone());
        let mut paths = verdicts
            .iter()
            .filter_map(|verdict| match verdict {
                InputVerdict::Violation(violation)
                    if violation.reason() == InputViolationReason::NoSuffix =>
                {
                    Some(violation.path().as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "by_name[\"entry\"].name",
                "config.name",
                "items[0].name",
                "name",
                "node.child.name"
            ],
            "{verdicts:?}"
        );
        let mut resource = Resource::with_provider("mock", "Review", "example", None);
        resource.attributes = fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect();
        let mut file = crate::parser::ExpandedInferredFile::default();
        file.resources.push(resource.clone()); // allow: direct — input-rule preview fixture
        let report = check_authored_inputs(&file, &schemas).unwrap();
        assert_eq!(report.len(), 5);
        assert!(report.pending().is_empty());
        assert!(
            report
                .iter()
                .all(|finding| finding.severity() == InputFindingSeverity::Error)
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved_input_fixture(resource, &schemas)));
        let rejected = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap_err();
        assert_eq!(rejected.errors().len(), 5);
    }
}

#[test]
fn preview_preserves_substitution_phase_and_reason() {
    use crate::resource::knowledge::{Classification, KnowledgeContext, PendingReason, Phase};
    use crate::resource::{LoopSlot, ParserPlaceholder};
    for (placeholder, reason) in [
        (
            ParserPlaceholder::ModuleArgument {
                name: "environment".into(),
                diagnostic: Default::default(),
            },
            PendingReason::ModuleArgument,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Key),
            PendingReason::LoopKey,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Index),
            PendingReason::LoopIndex,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Value),
            PendingReason::LoopValue,
        ),
    ] {
        for secret in [false, true] {
            let source: Value<ParserPlaceholder> =
                Value::Deferred(DeferredValue::Placeholder(placeholder.clone()));
            let source = if secret { source.into_secret() } else { source };
            let preview = source.input_preview();
            let knowledge = preview.knowledge(&KnowledgeContext::validation()).unwrap();
            let Classification::Pending(pending) = knowledge.into_classification() else {
                panic!("substitution stays pending");
            };
            assert_eq!((pending.phase(), pending.reason()), (Phase::Plan, &reason));
            assert!(!crate::value::format_value(&preview).contains("read after apply"));
        }
    }
    let template: Value<LoopSlot> = Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value));
    let preview = template.input_preview();
    let knowledge = preview.knowledge(&KnowledgeContext::validation()).unwrap();
    let Classification::Pending(pending) = knowledge.into_classification() else {
        panic!("template substitution stays pending");
    };
    assert_eq!(
        (pending.phase(), pending.reason()),
        (Phase::Plan, &PendingReason::LoopValue)
    );
}

#[test]
fn suffix_estimation_uses_the_expression_bound_after_list_reordering() {
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "items",
                AttributeType::list(AttributeType::struct_with_rules(
                    "Config",
                    vec![
                        StructField::optional("guard", AttributeType::bool()).build(),
                        StructField::optional("name", AttributeType::string()).build(),
                    ],
                    vec![],
                    vec![rule(
                        "name",
                        InputPredicate::NoSuffix(
                            crate::schema::NonEmptyString::new(".fifo").unwrap(),
                        ),
                        InputCondition::Equals {
                            path: path("guard"),
                            literal: InputLiteral::Bool(true),
                        },
                    )],
                )),
            )
            .build(),
        ),
    );
    let instance = |suffix: &str| {
        object([
            ("guard", Value::Concrete(ConcreteValue::Bool(true))),
            (
                "name",
                Value::Deferred(DeferredValue::Interpolation(vec![
                    crate::resource::InterpolationPart::Expr(Value::root_resource_ref(
                        "producer",
                        "name",
                        vec![],
                    )),
                    crate::resource::InterpolationPart::Literal(suffix.into()),
                ])),
            ),
        ])
    };
    let authored = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "items",
        Value::Concrete(ConcreteValue::List(vec![
            instance(".fifo"),
            instance(".standard"),
        ])),
    );
    let types = InputTypeContext::from_sources(
        &[crate::effect::deps::UnresolvedResource::from_pre_resolve(
            authored.clone(),
        )],
        &[],
        std::iter::empty(),
        &[],
        &schemas,
    );
    let actual = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "items",
        Value::Concrete(ConcreteValue::List(vec![
            instance(".standard"),
            instance(".fifo"),
        ])),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved_input_fixture(actual, &schemas)));
    let rejected = FinalizedPlan::new(plan)
        .check_inputs_with_types(&schemas, &HashMap::new(), &types)
        .unwrap_err();
    let paths = rejected
        .errors()
        .iter()
        .map(|error| match &error.kind {
            PlanErrorKind::InputRule { path, .. } => path.as_str(),
            other => panic!("unexpected finding: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["items[1].name"]);
}

fn conditional_list_schema(
    predicate: InputPredicate<crate::schema::PatternSpec>,
) -> SchemaRegistry {
    registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "items",
                AttributeType::list(AttributeType::struct_with_rules(
                    "Item",
                    vec![
                        StructField::optional("guard", AttributeType::bool()).build(),
                        StructField::optional("target", AttributeType::string()).build(),
                    ],
                    vec![],
                    vec![rule(
                        "target",
                        predicate,
                        InputCondition::Equals {
                            path: path("guard"),
                            literal: InputLiteral::Bool(true),
                        },
                    )],
                )),
            )
            .build(),
        ),
    )
}

#[test]
fn erased_reordered_instances_do_not_use_unmatched_authored_expressions() {
    let schemas = conditional_list_schema(InputPredicate::NoSuffix(
        crate::schema::NonEmptyString::new(".fifo").unwrap(),
    ));
    let authored_item = |guard, suffix: &str| {
        object([
            ("guard", Value::Concrete(ConcreteValue::Bool(guard))),
            (
                "target",
                Value::Deferred(DeferredValue::Interpolation(vec![
                    crate::resource::InterpolationPart::Expr(Value::root_resource_ref(
                        "producer",
                        "name",
                        vec![],
                    )),
                    crate::resource::InterpolationPart::Literal(suffix.into()),
                ])),
            ),
        ])
    };
    let authored = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "items",
        Value::Concrete(ConcreteValue::List(vec![
            authored_item(false, ".fifo"),
            authored_item(true, ".standard"),
        ])),
    );
    let types = InputTypeContext::from_sources(
        &[crate::effect::deps::UnresolvedResource::from_pre_resolve(
            authored,
        )],
        &[],
        std::iter::empty(),
        &[],
        &schemas,
    );
    let actual = Resource::with_provider("mock", "Review", "example", None).with_attribute(
        "items",
        Value::Concrete(ConcreteValue::List(vec![
            object([
                ("guard", Value::Concrete(ConcreteValue::Bool(true))),
                ("target", pending()),
            ]),
            object([
                ("guard", Value::Concrete(ConcreteValue::Bool(false))),
                ("target", pending()),
            ]),
        ])),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved_input_fixture(actual, &schemas)));
    let checked = FinalizedPlan::new(plan)
        .check_inputs_with_types(&schemas, &HashMap::new(), &types)
        .expect("unmatched authored instances cannot reject a valid normalized create");
    assert_eq!(checked.pending_inputs().len(), 1);
    let rows = checked.pending_rows(0);
    assert!(
        matches!(&rows[..], [crate::detail_rows::DetailRow::PendingInput { path, .. }] if path.as_str() == "items[0].target")
    );
}

#[test]
fn conditional_list_evaluation_has_a_linear_classification_budget() {
    const COUNT: usize = 2_000;
    let schemas = conditional_list_schema(InputPredicate::Present);
    let items = (0..COUNT)
        .map(|index| {
            object([
                (
                    "guard",
                    Value::Concrete(ConcreteValue::Bool(index % 2 == 0)),
                ),
                (
                    "target",
                    Value::Concrete(ConcreteValue::String("valid".into())),
                ),
            ])
        })
        .collect();
    crate::resource::knowledge::with_classification_budget(COUNT * 4 + 100, || {
        let verdicts = evaluate(
            &schemas,
            [("items", Value::Concrete(ConcreteValue::List(items)))],
        );
        assert_eq!(verdicts.len(), COUNT);
        assert!(
            verdicts
                .iter()
                .all(|verdict| *verdict == InputVerdict::Proven)
        );
    });
}

#[test]
fn failed_sibling_payloads_do_not_poison_unselected_targets_or_guards() {
    let schemas = conditional_list_schema(InputPredicate::Present);
    let verdicts = evaluate(
        &schemas,
        [(
            "items",
            Value::Concrete(ConcreteValue::List(vec![
                object([
                    ("guard", Value::Concrete(ConcreteValue::Bool(false))),
                    ("target", failed_value()),
                ]),
                object([
                    ("guard", Value::Concrete(ConcreteValue::Bool(false))),
                    (
                        "target",
                        Value::Concrete(ConcreteValue::String("valid".into())),
                    ),
                ]),
            ])),
        )],
    );
    assert_eq!(verdicts, vec![InputVerdict::Proven, InputVerdict::Proven]);
}

#[test]
fn collection_evidence_keeps_selected_failures_and_enclosing_computation_failures() {
    let schemas = conditional_list_schema(InputPredicate::Present);
    let item = |guard, target| {
        object([
            ("guard", Value::Concrete(ConcreteValue::Bool(guard))),
            ("target", target),
        ])
    };
    for secret in [false, true] {
        let protect = |value: Value| if secret { value.into_secret() } else { value };
        let verdicts = evaluate(
            &schemas,
            [(
                "items",
                protect(Value::Concrete(ConcreteValue::List(vec![
                    item(true, failed_value()),
                    item(
                        false,
                        Value::Concrete(ConcreteValue::String("valid".into())),
                    ),
                ]))),
            )],
        );
        assert_failed(&verdicts[0]);
        assert_eq!(verdicts[1], InputVerdict::Proven);

        let computed = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::Concrete(ConcreteValue::List(vec![
                item(false, failed_value()),
                item(
                    false,
                    Value::Concrete(ConcreteValue::String("valid".into())),
                ),
            ]))),
            default: Box::new(Value::Concrete(ConcreteValue::List(vec![]))),
        });
        let verdicts = evaluate(&schemas, [("items", protect(computed))]);
        assert_eq!(verdicts.len(), 2);
        for verdict in verdicts {
            assert_failed(&verdict);
        }
    }
}

#[test]
fn guard_selection_keeps_unrelated_failed_fields_out_of_resource_struct_and_map_rules() {
    let config = || {
        AttributeType::struct_with_rules(
            "Config",
            vec![
                StructField::optional("guard", AttributeType::bool()).build(),
                StructField::optional("target", AttributeType::string()).build(),
            ],
            vec![],
            vec![rule(
                "target",
                InputPredicate::Present,
                InputCondition::Equals {
                    path: path("guard"),
                    literal: InputLiteral::Bool(true),
                },
            )],
        )
    };
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("config", config()).build())
            .attribute(AttributeSchema::optional("entries", AttributeType::map(config())).build())
            .input_rule(rule(
                "config.target",
                InputPredicate::Present,
                InputCondition::Equals {
                    path: path("config.guard"),
                    literal: InputLiteral::Bool(true),
                },
            )),
    );
    for secret in [false, true] {
        let protect = |value: Value| if secret { value.into_secret() } else { value };
        let item = || {
            object([
                ("guard", Value::Concrete(ConcreteValue::Bool(false))),
                ("target", failed_value()),
            ])
        };
        let verdicts = evaluate(
            &schemas,
            [
                ("config", protect(item())),
                (
                    "entries",
                    protect(object([("one", item()), ("two", item())])),
                ),
            ],
        );
        assert_eq!(verdicts, vec![InputVerdict::Proven; 4]);
    }
}

fn fallback_tags_schema() -> SchemaRegistry {
    registry(
        ResourceSchema::new("Review")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(AttributeSchema::optional("fallback", AttributeType::string()).build())
            .input_rule(rule(
                "fallback",
                InputPredicate::Present,
                InputCondition::Not(Box::new(InputCondition::Present(path("tags")))),
            )),
    )
}

#[test]
fn possible_default_tags_keep_authored_absence_conditions_pending() {
    let schemas = fallback_tags_schema();
    let mut file = crate::parser::parse_file(
        "provider mock {}\nlet example = mock.Review {}",
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    file.providers[0].default_tags.insert(
        "app".into(),
        Value::resource_ref(
            crate::resource::RelativeBindingName::new("producer"),
            "value",
            vec![],
        ),
    );
    let report = check_authored_inputs(&file, &schemas).unwrap();
    assert!(
        report.is_empty(),
        "possible tags cannot prove the absence guard: {:?}",
        report.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    assert_eq!(report.pending().len(), 1);
}

#[test]
fn possible_default_tags_keep_plan_absence_conditions_pending() {
    let schemas = fallback_tags_schema();
    let target = Resource::with_provider("mock", "Review", "example", None);
    let config = crate::parser::ProviderConfig {
        name: "mock".into(),
        attributes: Default::default(),
        default_tags: IndexMap::from([(
            "app".into(),
            Value::root_resource_ref("producer", "value", vec![]),
        )]),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: Default::default(),
        binding: None,
    };
    let types = InputTypeContext::<ModuleSource>::empty()
        .with_default_tags(&[config], &schemas, [&target.id])
        .unwrap();
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved_input_fixture(target, &schemas)));
    let checked = FinalizedPlan::new(plan)
        .check_inputs_with_types(&schemas, &HashMap::new(), &types)
        .expect("possible default tags must not establish absence");
    assert_eq!(checked.pending_inputs().len(), 1);
}

#[test]
fn default_tag_membership_is_shared_by_presence_predicates_and_conditions() {
    for (target, predicate, condition) in [
        ("tags", InputPredicate::Present, InputCondition::Always),
        ("tags", InputPredicate::Absent, InputCondition::Always),
        (
            "fallback",
            InputPredicate::Present,
            InputCondition::Present(path("tags")),
        ),
        (
            "fallback",
            InputPredicate::Present,
            InputCondition::Not(Box::new(InputCondition::Present(path("tags")))),
        ),
    ] {
        let schemas = registry(
            ResourceSchema::new("Review")
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                )
                .attribute(AttributeSchema::optional("fallback", AttributeType::string()).build())
                .input_rule(rule(target, predicate, condition)),
        );
        let resource = Resource::with_provider("mock", "Review", "example", None);
        let config = crate::parser::ProviderConfig {
            name: "mock".into(),
            attributes: Default::default(),
            default_tags: IndexMap::from([(
                "app".into(),
                Value::root_resource_ref("producer", "value", vec![]),
            )]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let types = InputTypeContext::<ModuleSource>::empty()
            .with_default_tags(&[config], &schemas, [&resource.id])
            .unwrap();
        let mut site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                .unwrap()
                .unwrap();
        site.types = Some(&types.facts);
        assert_eq!(
            site.applicable_rules()
                .map(|r| r.evaluate())
                .collect::<Vec<_>>(),
            vec![InputVerdict::Undecided]
        );
    }
}

#[test]
fn failed_collection_and_union_owners_keep_failure_separate_from_membership() {
    let local = || {
        AttributeType::struct_with_rules(
            "Item",
            vec![StructField::optional("target", AttributeType::string()).build()],
            vec![],
            vec![rule(
                "target",
                InputPredicate::Present,
                InputCondition::Always,
            )],
        )
    };
    let schemas = registry(
        ResourceSchema::new("Review")
            .attribute(AttributeSchema::optional("items", AttributeType::list(local())).build())
            .attribute(AttributeSchema::optional("objects", AttributeType::map(local())).build())
            .attribute(
                AttributeSchema::optional(
                    "choice",
                    AttributeType::union(vec![local(), AttributeType::bool()]),
                )
                .build(),
            ),
    );
    let verdicts = evaluate(
        &schemas,
        [
            ("items", failed_value()),
            ("objects", failed_value()),
            ("choice", failed_value()),
        ],
    );
    assert_eq!(verdicts.len(), 3);
    assert!(verdicts.iter().all(|verdict| matches!(verdict, InputVerdict::Violation(v) if v.reason() == InputViolationReason::EvaluationFailed)), "{verdicts:?}");
    assert_eq!(
        evaluate(
            &schemas,
            [
                ("items", pending()),
                ("objects", pending()),
                ("choice", pending())
            ]
        ),
        vec![InputVerdict::Undecided; 3]
    );
}

#[test]
fn enclosing_failures_reach_nested_rules_without_consuming_unknown_instances() {
    let leaf = AttributeType::struct_with_rules(
        "Leaf",
        vec![StructField::optional("target", AttributeType::string()).build()],
        vec![],
        vec![rule(
            "target",
            InputPredicate::Present,
            InputCondition::Always,
        )],
    );
    let node = AttributeType::struct_(
        "Node",
        vec![
            StructField::optional("child", AttributeType::ref_("Node")).build(),
            StructField::optional("leaf", leaf).build(),
        ],
    );
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def("Node", node)
            .attribute(
                AttributeSchema::optional(
                    "items",
                    AttributeType::list(AttributeType::ref_("Node")),
                )
                .build(),
            ),
    );
    let failed = evaluate(&schemas, [("items", failed_value())]);
    assert_eq!(failed.len(), 1);
    assert_failed(&failed[0]);
    assert_eq!(
        evaluate(&schemas, [("items", pending())]),
        vec![InputVerdict::Undecided]
    );
}

#[test]
fn recursive_union_binding_reuses_fits_with_a_linear_validator_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const COUNT: usize = 12;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let scalar = AttributeType::refined_int_with_validator(
        TypeIdentity::new(Some("mock"), ["Review"], "Counted"),
        None,
        crate::schema::validator(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }),
    );
    let node = AttributeType::struct_with_rules(
        "NodeObject",
        vec![
            StructField::optional("number", scalar).build(),
            StructField::optional("target", AttributeType::bool()).build(),
            StructField::optional("child", AttributeType::ref_("Node")).build(),
        ],
        vec![],
        vec![rule(
            "target",
            InputPredicate::Present,
            InputCondition::Always,
        )],
    );
    let schemas = registry(
        ResourceSchema::new("Review")
            .with_def(
                "Node",
                AttributeType::union(vec![node, AttributeType::bool()]),
            )
            .attribute(AttributeSchema::optional("node", AttributeType::ref_("Node")).build()),
    );
    let mut node = object([
        ("number", Value::Concrete(ConcreteValue::Int(1))),
        ("target", Value::Concrete(ConcreteValue::Bool(true))),
    ]);
    for _ in 1..COUNT {
        node = object([
            ("number", Value::Concrete(ConcreteValue::Int(1))),
            ("target", Value::Concrete(ConcreteValue::Bool(true))),
            ("child", node),
        ]);
    }
    calls.store(0, Ordering::Relaxed);
    let verdicts = evaluate(&schemas, [("node", node)]);
    assert_eq!(verdicts.len(), COUNT);
    assert!(verdicts.iter().all(|v| *v == InputVerdict::Proven));
    let actual = calls.load(Ordering::Relaxed);
    assert!(
        actual <= COUNT,
        "{actual} scalar validator calls exceed the linear budget {COUNT}"
    );
}

#[test]
fn deep_rule_reachability_is_shared_with_a_linear_schema_visit_budget() {
    const DEPTH: usize = 64;
    let mut ty = AttributeType::struct_with_rules(
        "Leaf",
        vec![StructField::optional("target", AttributeType::bool()).build()],
        vec![],
        vec![rule(
            "target",
            InputPredicate::Present,
            InputCondition::Always,
        )],
    );
    let mut value = object([("target", Value::Concrete(ConcreteValue::Bool(true)))]);
    for _ in 1..DEPTH {
        ty = AttributeType::struct_("Parent", vec![StructField::optional("child", ty).build()]);
        value = object([("child", value)]);
    }
    instances::with_schema_visit_budget(DEPTH + 8, || {
        let schemas = registry(
            ResourceSchema::new("Review").attribute(AttributeSchema::optional("root", ty).build()),
        );
        for _ in 0..4 {
            let resource = Resource::with_provider("mock", "Review", "example", None)
                .with_attribute("root", value.clone());
            let site =
                classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
                    .unwrap()
                    .unwrap();
            assert!(instances::has_declarations(&site));
            assert_eq!(
                site.applicable_rules()
                    .map(|r| r.evaluate())
                    .collect::<Vec<_>>(),
                vec![InputVerdict::Proven]
            );
        }
    });
}

#[test]
fn deferred_subtree_preserves_each_scope_specific_finding() {
    let declaration = |scope, target, code| {
        InputRule::value(
            scope,
            path(target),
            InputRuleCode::new(code).unwrap(),
            InputPredicate::Present,
        )
    };
    let schemas = registry(
        ResourceSchema::new("Review").attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::struct_with_rules(
                    "Settings",
                    vec![
                        StructField::optional("preview", AttributeType::string()).build(),
                        StructField::optional("write", AttributeType::string()).build(),
                    ],
                    vec![],
                    vec![
                        declaration(InputRuleScope::CreateOnly, "preview", "review.preview"),
                        declaration(InputRuleScope::EveryWrite, "write", "review.write"),
                    ],
                ),
            )
            .build(),
        ),
    );
    for failed in [true, false] {
        let mut file = crate::parser::parse_file(
            "provider mock {}\nlet example = mock.Review { settings = {} }",
            std::path::Path::new("main.crn"),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        for source in file.iter_all_resources_mut() {
            if let crate::parser::ResourceMut::Resource(resource) = source {
                resource.attributes.insert(
                    "settings".into(),
                    if failed {
                        failed_value()
                    } else {
                        Value::Deferred(DeferredValue::Unknown(
                            crate::resource::ApplyTimeUnknown::PendingReRead(
                                crate::resource::PendingReRead::not_found(),
                            ),
                        ))
                    },
                );
            }
        }
        let report = check_authored_inputs(&file, &schemas).unwrap();
        if failed {
            assert_eq!(
                report.len(),
                2,
                "grouping must retain each independent rule verdict"
            );
            for severity in [InputFindingSeverity::Warning, InputFindingSeverity::Error] {
                assert_eq!(
                    report
                        .iter()
                        .filter(|finding| finding.severity() == severity)
                        .count(),
                    1
                );
            }
            assert!(report.pending().is_empty());
        } else {
            assert!(report.is_empty());
            assert_eq!(
                report.pending().len(),
                1,
                "the unknown subtree has only one pending obligation"
            );
        }
    }
}
