//! Portable sibling conditions share admission, evaluation and transport (#4263).
use std::collections::HashMap;

use carina_core::{
    effect::Effect,
    input_rules::check_authored_inputs,
    plan::{Plan, PlanErrorKind},
    protocol_convert::{protocol_to_resource_schema, resource_schema_to_protocol},
    provider::AppliedIdentifier,
    resource::{ConcreteValue, ResolvedResource, Resource, State, Value},
    schema::{
        AttributeSchema, AttributeType, InputCondition, InputLiteral, InputPath, InputPredicate,
        InputRule, InputRuleCode, InputRuleError, InputRuleScope, NonEmptyString, ResourceSchema,
        SchemaRegistry, StructField,
    },
};

#[path = "support/creation.rs"]
#[allow(dead_code)]
mod fixture;

fn path(value: &str) -> InputPath {
    InputPath::new(value).unwrap()
}
fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn boolean(value: bool) -> Value {
    Value::Concrete(ConcreteValue::Bool(value))
}
fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        fields.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    ))
}
fn pending() -> Value {
    Value::Deferred(carina_core::resource::DeferredValue::Unknown(
        carina_core::resource::ApplyTimeUnknown::PendingReRead(
            carina_core::resource::PendingReRead::not_found(),
        ),
    ))
}
fn rule(
    target: &str,
    predicate: InputPredicate<carina_core::schema::PatternSpec>,
    condition: InputCondition,
) -> InputRule {
    InputRule::value(
        InputRuleScope::EveryWrite,
        path(target),
        InputRuleCode::new("test.dependency").unwrap(),
        predicate,
    )
    .with_condition(condition)
}
fn schema() -> ResourceSchema {
    ResourceSchema::new("test.Conditional")
        .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
}
fn registry(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    schemas
}
fn desired(
    schemas: &SchemaRegistry,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> ResolvedResource {
    let mut resource = Resource::with_provider("mock", "test.Conditional", "example", None);
    resource.attributes = fields.into_iter().map(|(k, v)| (k.into(), v)).collect();
    ResolvedResource::resolve_raw_fixture(resource, schemas).unwrap()
}
fn check(
    schemas: &SchemaRegistry,
    resource: ResolvedResource,
) -> Result<
    carina_core::input_rules::InputCheckedPlan,
    carina_core::input_rules::InputAdmissionFailure,
> {
    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));
    fixture::finalize(plan).check_inputs(schemas, &HashMap::new())
}
fn equals_guard() -> InputCondition {
    InputCondition::Equals {
        path: path("guard"),
        literal: InputLiteral::Bool(true),
    }
}

#[test]
fn admission_rejects_guard_path_type_depth_and_duplicates() {
    let cases = [
        (
            InputCondition::Present(path("missing")),
            InputRuleError::UnknownPath,
        ),
        (
            InputCondition::Equals {
                path: path("guard"),
                literal: InputLiteral::String("true".into()),
            },
            InputRuleError::LiteralTypeMismatch,
        ),
        (
            (0..40).fold(InputCondition::Always, |c, _| {
                InputCondition::Not(Box::new(c))
            }),
            InputRuleError::ConditionTooDeep,
        ),
    ];
    for (condition, expected) in cases {
        let errors = schema()
            .input_rule(rule("target", InputPredicate::Present, condition))
            .validate()
            .unwrap_err();
        assert!(
            errors.input_rule_errors().any(|(_, e)| *e == expected),
            "{errors}"
        );
    }
    let declaration = rule("target", InputPredicate::Present, equals_guard());
    let errors = schema()
        .input_rule(declaration.clone())
        .input_rule(declaration)
        .validate()
        .unwrap_err();
    assert!(
        errors
            .input_rule_errors()
            .any(|(_, e)| *e == InputRuleError::DuplicateRule)
    );
}

#[test]
fn present_distinguishes_absence_false_and_pending_payload() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::Present,
        InputCondition::Present(path("guard")),
    )));
    assert!(check(&schemas, desired(&schemas, [])).is_ok());
    for guard in [boolean(false), boolean(true), pending()] {
        let error = check(&schemas, desired(&schemas, [("guard", guard)])).unwrap_err();
        assert!(
            matches!(&error.errors()[0].kind, PlanErrorKind::InputRule { path, reason, .. }
            if path == "target" && reason == "must be present when guard is present")
        );
    }
}

#[test]
fn equals_on_absence_is_false_and_not_preserves_pending() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::Present,
        InputCondition::Not(Box::new(equals_guard())),
    )));
    assert!(check(&schemas, desired(&schemas, [])).is_err());
    assert!(check(&schemas, desired(&schemas, [("guard", boolean(true))])).is_ok());
    let result = check(
        &schemas,
        desired(
            &schemas,
            [("guard", pending()), ("target", string("valid"))],
        ),
    )
    .unwrap();
    assert_eq!(result.pending_inputs().len(), 1);
}

#[test]
fn target_equals_rejects_missing_and_compares_known_scalars() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::Equals(InputLiteral::String("required".into())),
        equals_guard(),
    )));
    assert!(check(&schemas, desired(&schemas, [("guard", boolean(true))])).is_err());
    assert!(
        check(
            &schemas,
            desired(
                &schemas,
                [("guard", boolean(true)), ("target", string("other"))]
            )
        )
        .is_err()
    );
    assert!(
        check(
            &schemas,
            desired(
                &schemas,
                [("guard", boolean(true)), ("target", string("required"))]
            )
        )
        .is_ok()
    );
}

fn nested_schema() -> ResourceSchema {
    ResourceSchema::new("test.Conditional").attribute(
        AttributeSchema::optional(
            "items",
            AttributeType::list(AttributeType::struct_with_rules(
                "Pitr",
                vec![
                    StructField::optional("period", AttributeType::int()).build(),
                    StructField::optional("enabled", AttributeType::bool()).build(),
                ],
                vec![],
                vec![rule(
                    "enabled",
                    InputPredicate::Present,
                    InputCondition::Present(path("period")),
                )],
            )),
        )
        .build(),
    )
}

#[test]
fn struct_rules_run_per_list_instance_with_concrete_target_paths() {
    let schemas = registry(nested_schema());
    let items = Value::Concrete(ConcreteValue::List(vec![
        object([
            ("period", Value::Concrete(ConcreteValue::Int(7))),
            ("enabled", boolean(false)),
        ]),
        object([("period", Value::Concrete(ConcreteValue::Int(7)))]),
        object([]),
    ]));
    let error = check(&schemas, desired(&schemas, [("items", items)])).unwrap_err();
    assert_eq!(error.errors().len(), 1);
    assert!(
        matches!(&error.errors()[0].kind, PlanErrorKind::InputRule { path, .. } if path == "items[1].enabled")
    );
}

#[test]
fn required_conditions_and_struct_rules_survive_json_round_trip() {
    let original = nested_schema().validate().unwrap();
    let wire = resource_schema_to_protocol(&original).unwrap();
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(
        json["attributes"]["items"]["attr_type"]["element_type"]["input_rules"][0]["condition"]["kind"],
        "present"
    );
    let decoded = protocol_to_resource_schema(&serde_json::from_value(json).unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(resource_schema_to_protocol(&original).unwrap()).unwrap(),
        serde_json::to_value(resource_schema_to_protocol(&decoded).unwrap()).unwrap()
    );
}

#[test]
fn update_uses_unchanged_desired_guard_and_activates_on_guard_removal() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::NoSuffix(NonEmptyString::new(".fifo").unwrap()),
        InputCondition::Not(Box::new(equals_guard())),
    )));
    for (fields, changed) in [
        (
            vec![("guard", boolean(false)), ("target", string("queue.fifo"))],
            vec!["target".into()],
        ),
        (vec![("target", string("queue.fifo"))], vec!["guard".into()]),
    ] {
        let to = desired(&schemas, fields);
        let from = State::existing(
            to.id.clone(),
            HashMap::from([
                ("guard".into(), boolean(true)),
                ("target".into(), string("old")),
            ]),
        )
        .with_identifier("existing");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from.into()),
            identifier: AppliedIdentifier::new("existing").unwrap(),
            to,
            changed_attributes: changed,
        });
        let error = fixture::finalize(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap_err();
        assert!(
            matches!(&error.errors()[0].kind, PlanErrorKind::InputRule { path, .. } if path == "target")
        );
    }
}

#[test]
fn sqs_fifo_iff_uses_two_existing_string_predicates() {
    let mut draft = schema();
    for (code, condition, predicate) in [
        (
            "test.fifo",
            equals_guard(),
            InputPredicate::Matches(carina_core::schema::PatternSpec::new(
                "\\.fifo$",
                carina_core::schema::PatternMatch::Substring {
                    reason: carina_core::schema::PatternReason::new("FIFO suffix").unwrap(),
                },
            )),
        ),
        (
            "test.standard",
            InputCondition::Not(Box::new(equals_guard())),
            InputPredicate::NoSuffix(NonEmptyString::new(".fifo").unwrap()),
        ),
    ] {
        draft = draft.input_rule(
            InputRule::value(
                InputRuleScope::CreateOnly,
                path("target"),
                InputRuleCode::new(code).unwrap(),
                predicate,
            )
            .with_condition(condition),
        );
    }
    let schemas = registry(draft);
    for (fifo, name, valid) in [
        (true, "queue.fifo", true),
        (true, "queue", false),
        (false, "queue.fifo", false),
        (false, "queue", true),
    ] {
        assert_eq!(
            check(
                &schemas,
                desired(
                    &schemas,
                    [("guard", boolean(fifo)), ("target", string(name))]
                )
            )
            .is_ok(),
            valid
        );
    }
}

#[test]
fn authored_every_write_conditions_use_the_shared_report() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::Present,
        InputCondition::Present(path("guard")),
    )));
    let file = carina_core::parser::parse_file(
        "mock.test.Conditional { guard = false }",
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let findings = check_authored_inputs(&file, &schemas).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0]
            .to_string()
            .ends_with("target [test.dependency]: must be present when guard is present")
    );
}

fn enumeration(
    identity: &carina_core::schema::TypeIdentity,
    pairs: &[(&str, &str)],
) -> AttributeType {
    AttributeType::enum_(
        identity.clone(),
        Some(pairs.iter().map(|(api, _)| (*api).into()).collect()),
        pairs
            .iter()
            .map(|(api, dsl)| ((*api).into(), (*dsl).into()))
            .collect(),
        None,
        None,
    )
}
fn enum_literal(identity: &carina_core::schema::TypeIdentity, api: &str) -> InputLiteral {
    InputLiteral::Enum {
        identity: identity.clone(),
        api_value: api.into(),
    }
}
fn enum_value(raw: &str) -> Value {
    Value::Concrete(ConcreteValue::EnumIdentifier(
        carina_core::resource::RawEnumIdentifier::parse(raw),
    ))
}

#[test]
fn admission_checks_enum_identity_membership_and_canonical_spelling() {
    use carina_core::schema::TypeIdentity;
    let identity = TypeIdentity::new(Some("mock"), ["test", "Queue"], "Mode");
    let other = TypeIdentity::new(Some("other"), ["test", "Queue"], "Mode");
    for (literal, expected) in [
        (
            enum_literal(&other, "perMessageGroupId"),
            InputRuleError::EnumIdentityMismatch,
        ),
        (
            enum_literal(&identity, "missing"),
            InputRuleError::InvalidEnumLiteral,
        ),
        (
            enum_literal(&identity, "per_message_group_id"),
            InputRuleError::InvalidEnumLiteral,
        ),
        (
            InputLiteral::String("perMessageGroupId".into()),
            InputRuleError::LiteralTypeMismatch,
        ),
    ] {
        let errors = ResourceSchema::new("test.Conditional")
            .attribute(
                AttributeSchema::optional(
                    "guard",
                    enumeration(&identity, &[("perMessageGroupId", "per_message_group_id")]),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule(
                "target",
                InputPredicate::Present,
                InputCondition::Equals {
                    path: path("guard"),
                    literal,
                },
            ))
            .validate()
            .unwrap_err();
        assert!(
            errors.input_rule_errors().any(|(_, e)| *e == expected),
            "{errors}"
        );
    }
}

#[test]
fn admission_checks_writability_predicate_compatibility_and_unsupported_literals() {
    let base = || {
        ResourceSchema::new("test.Conditional")
            .attribute(
                AttributeSchema::optional("guard", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("output", AttributeType::string()).build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
    };
    for (declaration, expected) in [
        (
            rule("missing", InputPredicate::Present, InputCondition::Always),
            InputRuleError::UnknownPath,
        ),
        (
            rule("output", InputPredicate::Present, InputCondition::Always),
            InputRuleError::ProviderPopulatedPath,
        ),
        (
            rule(
                "target",
                InputPredicate::Present,
                InputCondition::Present(path("output")),
            ),
            InputRuleError::ProviderPopulatedPath,
        ),
        (
            rule(
                "target",
                InputPredicate::Present,
                InputCondition::Equals {
                    path: path("guard"),
                    literal: InputLiteral::String("{}".into()),
                },
            ),
            InputRuleError::UnsupportedLiteral,
        ),
        (
            InputRule::new(
                InputRuleScope::EveryWrite,
                carina_core::schema::InputRuleTarget::MapKeys(path("guard")),
                InputRuleCode::new("test.keys").unwrap(),
                InputPredicate::Present,
            ),
            InputRuleError::IncompatiblePredicate,
        ),
    ] {
        let errors = base().input_rule(declaration).validate().unwrap_err();
        assert!(
            errors.input_rule_errors().any(|(_, e)| *e == expected),
            "{errors}"
        );
    }
}

#[test]
fn sqs_throughput_enum_aliases_share_guard_and_target_equality() {
    use carina_core::schema::TypeIdentity;
    let throughput = TypeIdentity::new(Some("mock"), ["test", "Queue"], "Throughput");
    let dedup = TypeIdentity::new(Some("mock"), ["test", "Queue"], "Dedup");
    let schemas = registry(
        ResourceSchema::new("test.Conditional")
            .attribute(
                AttributeSchema::optional(
                    "guard",
                    enumeration(
                        &throughput,
                        &[
                            ("perMessageGroupId", "per_message_group_id"),
                            ("perQueue", "per_queue"),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "target",
                    enumeration(
                        &dedup,
                        &[("messageGroup", "message_group"), ("queue", "queue")],
                    ),
                )
                .build(),
            )
            .input_rule(rule(
                "target",
                InputPredicate::Equals(enum_literal(&dedup, "messageGroup")),
                InputCondition::Equals {
                    path: path("guard"),
                    literal: enum_literal(&throughput, "perMessageGroupId"),
                },
            )),
    );
    for guard in [
        enum_value("mock.test.Queue.Throughput.per_message_group_id"),
        string("perMessageGroupId"),
    ] {
        for target in [
            enum_value("mock.test.Queue.Dedup.message_group"),
            string("messageGroup"),
        ] {
            assert!(
                check(
                    &schemas,
                    desired(&schemas, [("guard", guard.clone()), ("target", target)])
                )
                .is_ok()
            );
        }
        assert!(
            check(
                &schemas,
                desired(&schemas, [("guard", guard), ("target", string("queue"))])
            )
            .is_err()
        );
    }
    assert!(
        check(
            &schemas,
            desired(
                &schemas,
                [("guard", string("perQueue")), ("target", string("queue"))]
            )
        )
        .is_ok()
    );
}

#[test]
fn sqs_redrive_presence_uses_ordinary_list_bounds() {
    use carina_core::schema::{ListElements, TypeIdentity};
    let permission = TypeIdentity::new(Some("mock"), ["test", "Policy"], "Permission");
    let by_queue = InputCondition::Equals {
        path: path("permission"),
        literal: enum_literal(&permission, "byQueue"),
    };
    let present = InputRule::value(
        InputRuleScope::EveryWrite,
        path("arns"),
        InputRuleCode::new("test.by_queue_present").unwrap(),
        InputPredicate::Present,
    )
    .with_condition(by_queue.clone());
    let absent = InputRule::value(
        InputRuleScope::EveryWrite,
        path("arns"),
        InputRuleCode::new("test.not_by_queue_absent").unwrap(),
        InputPredicate::Absent,
    )
    .with_condition(InputCondition::Not(Box::new(by_queue)));
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(
            AttributeSchema::optional(
                "policy",
                AttributeType::struct_with_rules(
                    "Policy",
                    vec![
                        StructField::optional(
                            "permission",
                            enumeration(
                                &permission,
                                &[("byQueue", "by_queue"), ("allowAll", "allow_all")],
                            ),
                        )
                        .build(),
                        StructField::optional(
                            "arns",
                            AttributeType::refined_list(
                                AttributeType::string(),
                                true,
                                Some((Some(1), Some(10))),
                                ListElements::AllowDuplicates,
                            ),
                        )
                        .build(),
                    ],
                    vec![],
                    vec![present, absent],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let list = |count| {
        Value::Concrete(ConcreteValue::List(
            (0..count).map(|_| string("arn:queue")).collect(),
        ))
    };
    for count in [0, 1, 10, 11] {
        let value = object([
            (
                "permission",
                enum_value("mock.test.Policy.Permission.by_queue"),
            ),
            ("arns", list(count)),
        ]);
        let result = schema
            .effective_attribute("policy")
            .unwrap()
            .validate(&value);
        assert_eq!(
            result.is_ok(),
            (1..=10).contains(&count),
            "count={count}: {result:?}"
        );
    }
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema);
    for (permission, arns, valid) in [
        ("byQueue", Some(list(1)), true),
        ("byQueue", None, false),
        ("allowAll", None, true),
        ("allowAll", Some(list(1)), false),
    ] {
        let mut values = vec![("permission", string(permission))];
        if let Some(arns) = arns {
            values.push(("arns", arns));
        }
        assert_eq!(
            check(&schemas, desired(&schemas, [("policy", object(values))])).is_ok(),
            valid
        );
    }
}

#[test]
fn s3_account_regional_suffix_is_create_only_and_imports_are_excluded() {
    use carina_core::schema::TypeIdentity;
    let namespace = TypeIdentity::new(Some("mock"), ["s3", "Bucket"], "Namespace");
    let schemas = registry(
        ResourceSchema::new("test.Conditional")
            .attribute(
                AttributeSchema::optional(
                    "guard",
                    enumeration(
                        &namespace,
                        &[
                            ("account-regional", "account_regional"),
                            ("global", "global"),
                        ],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(
                InputRule::value(
                    InputRuleScope::CreateOnly,
                    path("target"),
                    InputRuleCode::new("s3.bucket.namespace_suffix").unwrap(),
                    InputPredicate::NoSuffix(NonEmptyString::new("-an").unwrap()),
                )
                .with_condition(InputCondition::Not(Box::new(
                    InputCondition::Equals {
                        path: path("guard"),
                        literal: enum_literal(&namespace, "account-regional"),
                    },
                ))),
            ),
    );
    for (guard, valid) in [
        (None, false),
        (Some(string("global")), false),
        (Some(string("account-regional")), true),
    ] {
        let mut fields = vec![("target", string("bucket-an"))];
        if let Some(guard) = guard {
            fields.push(("guard", guard));
        }
        assert_eq!(check(&schemas, desired(&schemas, fields)).is_ok(), valid);
    }
    let to = desired(&schemas, [("target", string("bucket-an"))]);
    let mut update = Plan::new();
    update.add(Effect::Update {
        from: Box::new(
            State::existing(
                to.id.clone(),
                HashMap::from([("target".into(), string("old"))]),
            )
            .with_identifier("existing")
            .into(),
        ),
        identifier: AppliedIdentifier::new("existing").unwrap(),
        to: to.clone(),
        changed_attributes: vec!["target".into()],
    });
    assert!(
        fixture::finalize(update)
            .check_inputs(&schemas, &HashMap::new())
            .is_ok()
    );
    let mut import = Plan::new();
    import.add(Effect::Import {
        id: to.resolved_id(),
        identifier: string("existing"),
    });
    assert!(
        fixture::finalize(import)
            .check_inputs(&schemas, &HashMap::new())
            .is_ok()
    );
    let file = carina_core::parser::parse_file(
        "mock.test.Conditional { target = 'bucket-an' }",
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert!(check_authored_inputs(&file, &schemas).unwrap().is_empty());
}

#[test]
fn recursive_defs_and_map_instances_keep_local_ownership() {
    let definition = AttributeType::struct_with_rules(
        "Node",
        vec![
            StructField::optional("guard", AttributeType::int()).build(),
            StructField::optional("target", AttributeType::bool()).build(),
            StructField::optional("children", AttributeType::list(AttributeType::ref_("Node")))
                .build(),
        ],
        vec![],
        vec![rule(
            "target",
            InputPredicate::Present,
            InputCondition::Present(path("guard")),
        )],
    );
    let draft = ResourceSchema::new("test.Conditional")
        .with_def("Node", definition)
        .attribute(
            AttributeSchema::optional("nodes", AttributeType::map(AttributeType::ref_("Node")))
                .build(),
        );
    let admitted = draft.validate().unwrap();
    let json = serde_json::to_value(resource_schema_to_protocol(&admitted).unwrap()).unwrap();
    assert_eq!(
        json["defs"]["Node"]["input_rules"][0]["condition"]["kind"],
        "present"
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        protocol_to_resource_schema(&serde_json::from_value(json).unwrap()).unwrap(),
    );
    let children = Value::Concrete(ConcreteValue::List(vec![object([(
        "guard",
        Value::Concrete(ConcreteValue::Int(1)),
    )])]));
    let result = check(
        &schemas,
        desired(
            &schemas,
            [(
                "nodes",
                object([("root", object([("children", children)]))]),
            )],
        ),
    )
    .unwrap_err();
    assert!(
        matches!(&result.errors()[0].kind,PlanErrorKind::InputRule {path,..} if path == "nodes[\"root\"].children[0].target")
    );
}

#[test]
fn pending_parent_membership_is_not_proven_absence() {
    let draft = ResourceSchema::new("test.Conditional")
        .attribute(
            AttributeSchema::optional(
                "guard",
                AttributeType::struct_(
                    "Guard",
                    vec![StructField::optional("flag", AttributeType::bool()).build()],
                ),
            )
            .build(),
        )
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
        .input_rule(rule(
            "target",
            InputPredicate::Present,
            InputCondition::Not(Box::new(InputCondition::Present(path("guard.flag")))),
        ));
    let schemas = registry(draft);
    assert_eq!(
        check(&schemas, desired(&schemas, [("guard", pending())]))
            .unwrap()
            .pending_inputs()
            .len(),
        1
    );
    assert!(
        check(
            &schemas,
            desired(&schemas, [("guard", object([("flag", pending())]))])
        )
        .is_ok()
    );
    assert!(check(&schemas, desired(&schemas, [("guard", object([]))])).is_err());
}

#[test]
fn protected_pending_guards_do_not_pass() {
    let schemas =
        registry(schema().input_rule(rule("target", InputPredicate::Present, equals_guard())));
    assert!(
        check(
            &schemas,
            desired(
                &schemas,
                [(
                    "guard",
                    Value::Deferred(carina_core::resource::DeferredValue::Secret(
                        Box::new(boolean(true)).into()
                    ))
                )]
            )
        )
        .is_err()
    );
    assert_eq!(
        check(
            &schemas,
            desired(
                &schemas,
                [(
                    "guard",
                    Value::Deferred(carina_core::resource::DeferredValue::Secret(
                        Box::new(pending()).into()
                    ))
                )]
            )
        )
        .unwrap()
        .pending_inputs()
        .len(),
        1
    );
}

#[test]
fn update_target_removal_is_absence_but_an_omitted_guard_has_no_observed_evidence() {
    let schemas =
        registry(schema().input_rule(rule("target", InputPredicate::Present, equals_guard())));
    for (fields, changed, violates, pending_count) in [
        (
            vec![("guard", boolean(true))],
            vec!["target".into()],
            true,
            0,
        ),
        (
            vec![("target", string("new"))],
            vec!["target".into()],
            false,
            1,
        ),
    ] {
        let to = desired(&schemas, fields);
        let from = State::existing(
            to.id.clone(),
            HashMap::from([
                ("guard".into(), boolean(true)),
                ("target".into(), string("old")),
            ]),
        )
        .with_identifier("existing");
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from.into()),
            identifier: AppliedIdentifier::new("existing").unwrap(),
            to,
            changed_attributes: changed,
        });
        match fixture::finalize(plan).check_inputs(&schemas, &HashMap::new()) {
            Ok(result) => {
                assert!(!violates);
                assert_eq!(result.pending_inputs().len(), pending_count);
            }
            Err(error) => {
                assert!(violates, "{error:?}");
                assert!(
                    matches!(&error.errors()[0].kind,PlanErrorKind::InputRule { path,.. } if path == "target")
                );
            }
        }
    }
}

#[test]
fn native_and_json_roundtrips_preserve_every_condition_and_scalar_kind() {
    use carina_core::schema::TypeIdentity;
    let identity = TypeIdentity::new(Some("mock"), ["test", "Conditional"], "Mode");
    for (ty, literal) in [
        (AttributeType::bool(), InputLiteral::Bool(true)),
        (AttributeType::int(), InputLiteral::Int(3)),
        (
            AttributeType::float(),
            InputLiteral::Float(carina_core::FiniteF64::new(1.5).unwrap()),
        ),
        (
            AttributeType::string(),
            InputLiteral::String("a\"b\n".into()),
        ),
        (
            enumeration(&identity, &[("messageGroup", "message_group")]),
            enum_literal(&identity, "messageGroup"),
        ),
    ] {
        for condition in [
            InputCondition::Always,
            InputCondition::Present(path("guard")),
            InputCondition::Equals {
                path: path("guard"),
                literal: literal.clone(),
            },
            InputCondition::Not(Box::new(InputCondition::Equals {
                path: path("guard"),
                literal: literal.clone(),
            })),
        ] {
            let original = ResourceSchema::new("test.Conditional")
                .attribute(AttributeSchema::optional("guard", ty.clone()).build())
                .attribute(AttributeSchema::optional("target", ty.clone()).build())
                .input_rule(rule(
                    "target",
                    InputPredicate::Equals(literal.clone()),
                    condition.clone(),
                ))
                .validate()
                .unwrap();
            assert_eq!(
                original.as_schema().input_rules()[0].condition(),
                &condition
            );
            let wire = resource_schema_to_protocol(&original).unwrap();
            let json = serde_json::to_string(&wire).unwrap();
            let decoded =
                protocol_to_resource_schema(&serde_json::from_str(&json).unwrap()).unwrap();
            assert_eq!(
                original.as_schema().input_rules(),
                decoded.as_schema().input_rules()
            );
        }
    }
}

#[test]
fn literal_admission_respects_refinements_and_struct_rule_ownership() {
    let draft = ResourceSchema::new("test.Conditional")
        .attribute(
            AttributeSchema::optional(
                "guard",
                AttributeType::refined_int(None, Some((Some(1), Some(10)))),
            )
            .build(),
        )
        .attribute(AttributeSchema::optional("target", AttributeType::bool()).build())
        .input_rule(rule(
            "target",
            InputPredicate::Present,
            InputCondition::Equals {
                path: path("guard"),
                literal: InputLiteral::Int(0),
            },
        ));
    assert!(
        draft
            .validate()
            .unwrap_err()
            .input_rule_errors()
            .any(|(_, e)| *e == InputRuleError::LiteralTypeMismatch)
    );
    let duplicate = rule(
        "target",
        InputPredicate::Present,
        InputCondition::Present(path("guard")),
    );
    let draft = ResourceSchema::new("test.Conditional").with_def(
        "Nested",
        AttributeType::struct_with_rules(
            "Nested",
            vec![
                StructField::optional("guard", AttributeType::bool()).build(),
                StructField::optional("target", AttributeType::string()).build(),
            ],
            vec![],
            vec![duplicate.clone(), duplicate],
        ),
    );
    let error = draft.validate().unwrap_err();
    assert!(error.input_rule_errors().any(|(location, e)| matches!(
        location,
        carina_core::schema::SchemaValidationIssueLocation::Definition(_)
    ) && *e
        == InputRuleError::DuplicateRule));
}

#[test]
fn authored_report_retains_pending_guards_as_obligations() {
    let schemas = registry(schema().input_rule(rule(
        "target",
        InputPredicate::Present,
        InputCondition::Not(Box::new(equals_guard())),
    )));
    let mut file = carina_core::parser::parse_file(
        "mock.test.Conditional { guard = true }",
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    file.resources[0].attributes.insert(
        "guard".into(),
        Value::Deferred(carina_core::resource::DeferredValue::Unknown(
            carina_core::resource::ApplyTimeUnknown::PendingReRead(
                carina_core::resource::PendingReRead::not_found(),
            ),
        )),
    );
    let report = check_authored_inputs(&file, &schemas).unwrap();
    assert!(report.is_empty());
    assert_eq!(report.pending().len(), 1);
    assert_eq!(report.pending()[0].path.as_str(), "target");
    assert_eq!(report.pending()[0].code.as_str(), "test.dependency");
}

#[test]
fn struct_rule_diagnostics_never_disclose_secret_map_keys() {
    let schemas = registry(
        ResourceSchema::new("test.Conditional").attribute(
            AttributeSchema::optional(
                "objects",
                AttributeType::map(AttributeType::struct_with_rules(
                    "Entry",
                    vec![
                        StructField::optional("guard", AttributeType::bool()).build(),
                        StructField::optional("target", AttributeType::string()).build(),
                    ],
                    vec![],
                    vec![rule(
                        "target",
                        InputPredicate::Present,
                        InputCondition::Present(path("guard")),
                    )],
                )),
            )
            .build(),
        ),
    );
    let entries = object([("private-map-key", object([("guard", boolean(true))]))]);
    let secret = Value::Deferred(carina_core::resource::DeferredValue::Secret(
        Box::new(entries).into(),
    ));
    let error = check(&schemas, desired(&schemas, [("objects", secret)])).unwrap_err();
    let diagnostic = format!("{:?}", error.errors());
    assert!(!diagnostic.contains("private-map-key"), "{diagnostic}");
    assert!(diagnostic.contains("objects"));
}
