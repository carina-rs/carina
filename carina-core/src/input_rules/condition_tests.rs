use super::*;
use crate::{
    builtins::BuiltinFunction,
    resource::{DiagnosticSourceLine, UnavailableReason},
    schema::{AttributeSchema, AttributeType, ResourceSchema},
};

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
        let site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
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
    use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment, ResolvedIgnoreChanges};
    use crate::schema::{LeafPath, LeafPathSegment, StructField};
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
            "settings",
            LeafPath::from_segments([LeafPathSegment::StructField("guard".into())]),
            ParsedIgnorePath::new(vec![
                ParsedIgnorePathSegment::Attribute("settings".into()),
                ParsedIgnorePathSegment::Attribute("guard".into()),
            ]),
        ),
    };
    update.overlay(&crate::provider::UpdatePatch {
        ops: vec![crate::provider::PatchOp::Replace {
            key: "settings".into(),
            value: object(false),
        }],
    });
    site.update = Some(update);
    assert_eq!(
        site.applicable_rules().next().unwrap().evaluate(),
        InputVerdict::Undecided
    );
}
