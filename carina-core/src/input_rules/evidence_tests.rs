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
        desired: resource.attributes.clone(),
        projected: IndexMap::new(),
        changed: HashSet::new(),
        removed: HashSet::new(),
        ignored: ResolvedIgnoreChanges::indexed_for_test(
            "guard",
            crate::schema::LeafPath::from_segments([]),
            ParsedIgnorePath::new(vec![ParsedIgnorePathSegment::Attribute("guard".into())]),
        ),
    };
    update.overlay(&crate::provider::UpdatePatch {
        ops: vec![crate::provider::PatchOp::Replace {
            key: "target".into(),
            value: resource.attributes["target"].clone(),
        }],
    });
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
