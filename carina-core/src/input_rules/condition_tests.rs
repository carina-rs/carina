use super::*;
use crate::{
    builtins::BuiltinFunction,
    resource::{DiagnosticSourceLine, UnavailableReason},
    schema::{AttributeSchema, AttributeType, ResourceSchema},
};

fn create_only_schema() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Conditional")
            .with_coexisting_replacement()
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("test.create_suffix").unwrap(),
                InputPredicate::NoSuffix(crate::schema::NonEmptyString::new("-an").unwrap()),
            ))
            .validate()
            .unwrap(),
    );
    schemas
}

#[test]
fn create_only_verdict_yields_authored_warning_and_create_error() {
    let schemas = create_only_schema();
    let resource =
        crate::resource::Resource::with_provider("mock", "test.Conditional", "example", None)
            .with_attribute(
                "target",
                Value::Concrete(ConcreteValue::String("bucket-an".into())),
            );
    let authored =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
            .unwrap()
            .unwrap();
    let authored_rule = authored.applicable_rules().next().unwrap();
    let InputRuleBinding::Instance(authored_rule) = authored_rule.binding else {
        panic!("concrete resource rule");
    };
    assert_eq!(authored_rule.kind, InputFindingKind::CreateOnlyPreview);
    let authored_verdict = authored_rule.evaluate();
    let InputVerdict::Violation(violation) = authored_verdict.clone() else {
        panic!("the authored rule must evaluate to the same violation as create");
    };
    let warning = authored_rule.finding(violation);
    assert_eq!(warning.kind, InputFindingKind::CreateOnlyPreview);
    assert_eq!(warning.severity(), InputFindingSeverity::Warning);
    assert_eq!(
        warning.to_string(),
        "mock.test.Conditional.example: target [test.create_suffix]: has a forbidden suffix (rejected if this resource is created)"
    );
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
    let create_rule = site.applicable_rules().next().unwrap();
    let InputRuleBinding::Instance(create_rule) = create_rule.binding else {
        panic!("concrete resource rule");
    };
    assert_eq!(create_rule.kind, InputFindingKind::Violation);
    let create_verdict = create_rule.evaluate();
    assert_eq!(create_verdict, authored_verdict);
    let InputVerdict::Violation(violation) = create_verdict else {
        unreachable!()
    };
    let error = create_rule.finding(violation);
    assert_eq!(error.kind, InputFindingKind::Violation);
    assert_eq!(error.severity(), InputFindingSeverity::Error);
    assert_eq!(
        error.to_string(),
        "mock.test.Conditional.example: target [test.create_suffix]: has a forbidden suffix"
    );
    let rejected = finalized.check_inputs(&schemas, &current).unwrap_err();
    assert_eq!(rejected.errors().len(), 1);
    assert!(
        matches!(&rejected.errors()[0].kind, PlanErrorKind::InputRule { reason, context: InputWriteContext::Create, .. } if reason == "has a forbidden suffix")
    );
}

#[test]
fn input_finding_kind_classifies_create_only_previews_and_other_writes() {
    use InputFindingKind::{CreateOnlyPreview, Violation};
    for (context, create_only) in [
        (InputWriteContext::AuthoredDesired, Some(CreateOnlyPreview)),
        (InputWriteContext::Create, Some(Violation)),
        (
            InputWriteContext::Replacement {
                create_before_destroy: false,
            },
            Some(Violation),
        ),
        (
            InputWriteContext::Replacement {
                create_before_destroy: true,
            },
            Some(Violation),
        ),
        (InputWriteContext::ResumeCreate, Some(Violation)),
        (InputWriteContext::DeferredCreate, Some(Violation)),
        (InputWriteContext::DeferredReplacement, Some(Violation)),
        (InputWriteContext::Update, None),
    ] {
        assert_eq!(
            scope_classification(InputRuleScope::CreateOnly, context),
            create_only
        );
        assert_eq!(
            scope_classification(InputRuleScope::EveryWrite, context),
            Some(Violation)
        );
    }
}

#[test]
fn create_only_plan_exempts_import_update_and_rejects_replacement_creates() {
    use crate::{
        effect::{ChangedCreateOnly, DeferredReplaceDelete, EffectGeneration},
        plan::ReplacementGroup,
        provider::AppliedIdentifier,
        resource::{ResolvedResource, Resource, State},
    };
    let schemas = create_only_schema();
    let resource = ResolvedResource::resolve_raw_fixture(
        Resource::with_provider("mock", "test.Conditional", "example", None).with_attribute(
            "target",
            Value::Concrete(ConcreteValue::String("bucket-an".into())),
        ),
        &schemas,
    )
    .unwrap();
    for effect in [
        Effect::Import {
            id: resource.resolved_id(),
            identifier: Value::Concrete(ConcreteValue::String("existing".into())),
        },
        Effect::Update {
            from: Box::new(
                State::existing(resource.id.clone(), HashMap::new()).with_identifier("existing"),
            ),
            identifier: AppliedIdentifier::new("existing").unwrap(),
            to: resource.clone(),
            changed_attributes: vec!["target".into()],
        },
    ] {
        let mut plan = Plan::new();
        plan.add(effect);
        let admitted = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap();
        assert!(admitted.errors().is_empty());
        assert!(admitted.pending_inputs().is_empty());
    }
    for create_before_destroy in [false, true] {
        let mut plan = Plan::new();
        let old = DeferredReplaceDelete {
            id: resource.resolved_id(),
            identifier: AppliedIdentifier::new("existing").unwrap(),
            generation: EffectGeneration::Current,
            previous_attributes: HashMap::new(),
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        plan.add_replacement(ReplacementGroup {
            replacement: crate::test_support::checked_replacement(
                resource.clone(),
                old,
                create_before_destroy,
            ),
            changed_create_only: ChangedCreateOnly::new(vec!["target".into()]).unwrap(),
            cascade_ref_hints: vec![],
            consumer_updates: Default::default(),
        });
        let rejected = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap_err();
        assert_eq!(rejected.errors().len(), 1);
        assert!(
            matches!(&rejected.errors()[0].kind, PlanErrorKind::InputRule { context: InputWriteContext::Replacement { create_before_destroy: actual }, reason, .. } if *actual == create_before_destroy && reason == "has a forbidden suffix")
        );
    }
}

#[test]
fn failed_guard_is_a_failure_even_under_negation_and_after_authored_projection() {
    let rule = InputRule::value(
        InputRuleScope::EveryWrite,
        InputPath::new("target").unwrap(),
        InputRuleCode::new("test.failed_guard").unwrap(),
        InputPredicate::Present,
    )
    .with_condition(InputCondition::Not(Box::new(InputCondition::Equals {
        path: InputPath::new("guard").unwrap(),
        literal: InputLiteral::Bool(true),
    })));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Conditional")
            .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule)
            .validate()
            .unwrap(),
    );
    let failed: Value = Value::Deferred(DeferredValue::Unavailable {
        name: BuiltinFunction::env,
        args: Vec::new(),
        reason: UnavailableReason::from_builtin_error("private attempted failure".into()),
        source_line: DiagnosticSourceLine::default(),
    });
    let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
        crate::resource::InterpolationPart::Expr(failed.clone()),
    ]));
    let function = Value::Deferred(DeferredValue::FunctionCall {
        name: BuiltinFunction::length,
        args: vec![crate::resource::FunctionArgument::from_value(
            failed.clone(),
        )],
        source_line: DiagnosticSourceLine::default(),
    });
    let coalesce = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(failed.clone()),
        default: Box::new(Value::Concrete(ConcreteValue::Bool(false))),
    });
    for value in [
        failed.clone(),
        failed.into_secret(),
        interpolation,
        function,
        coalesce,
    ] {
        let projected = project_known_value(&value);
        assert!(
            projected
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .unwrap()
                .failure()
                .is_some()
        );
        let resource =
            crate::resource::Resource::with_provider("mock", "test.Conditional", "example", None)
                .with_attribute("guard", projected);
        let site = classify_write_site::<ModuleSource>(
            WriteSiteSource::Preview {
                resource_id: resource.id.clone(),
                input: RuleInput::for_authored(
                    &AuthoredInputs::resource_for_test(&resource)
                        .resources()
                        .next()
                        .unwrap(),
                    schemas.get_for(&resource).unwrap().schema(),
                    Arc::new(TypeFacts::default()),
                    &schemas,
                )
                .unwrap(),
            },
            &schemas,
        )
        .unwrap()
        .unwrap();
        let verdict = site.applicable_rules().next().unwrap().evaluate();
        assert!(
            matches!(&verdict,InputVerdict::Violation(v) if v.reason() == crate::schema::InputViolationReason::EvaluationFailed)
        );
        assert!(!format!("{verdict:?}").contains("private attempted failure"));
    }
}

#[test]
fn update_guard_cannot_use_an_ignored_leaf_copied_from_observation() {
    use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment};
    use crate::schema::StructField;
    let rule = InputRule::value(
        InputRuleScope::EveryWrite,
        InputPath::new("target").unwrap(),
        InputRuleCode::new("test.ignored_guard").unwrap(),
        InputPredicate::NoSuffix(crate::schema::NonEmptyString::new(".fifo").unwrap()),
    )
    .with_condition(InputCondition::Equals {
        path: InputPath::new("settings.guard").unwrap(),
        literal: InputLiteral::Bool(true),
    });
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Conditional")
            .attribute(
                AttributeSchema::optional(
                    "settings",
                    AttributeType::struct_(
                        "Settings",
                        vec![StructField::optional("guard", AttributeType::bool()).build()],
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .input_rule(rule)
            .validate()
            .unwrap(),
    );
    let object = |guard| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "guard".into(),
            Value::Concrete(ConcreteValue::Bool(guard)),
        )])))
    };
    let resource =
        crate::resource::Resource::with_provider("mock", "test.Conditional", "example", None)
            .with_attribute(
                "target",
                Value::Concrete(ConcreteValue::String("queue.fifo".into())),
            )
            .with_attribute("settings", object(true));
    let mut resource = resource;
    resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
        ParsedIgnorePathSegment::Attribute("settings".into()),
        ParsedIgnorePathSegment::Attribute("guard".into()),
    ])];
    let previous = crate::resource::State::existing(
        resource.id.clone(),
        crate::test_support::snapshot_attributes::<crate::resource::NoPlaceholder>(HashMap::from(
            [
                ("settings".into(), object(false)),
                ("target".into(), resource.attributes["target"].clone()),
            ],
        )),
    )
    .with_identifier("old");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(previous),
        identifier: crate::provider::AppliedIdentifier::new("old").unwrap(),
        to: crate::resource::ResolvedResource::resolve_raw_fixture(resource, &schemas).unwrap(),
        changed_attributes: vec!["settings".into()],
    });
    let finalized = FinalizedPlan::new(plan);
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
}
