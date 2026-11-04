use super::*;
use crate::parser::ProviderContext;
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
};
use crate::static_type::{InferenceError, InferenceIssue};
use crate::validation::typecheck::{TypecheckInference, check_type_expr_value_providerless};

fn config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

#[test]
fn heterogeneous_record_keeps_its_field_constraint_checks() {
    let parsed =
        crate::parser::parse_and_resolve("let value = { label = 'x' inner = { leaf = 'bad' } }")
            .unwrap();
    let config = config();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config);
    assert!(report.findings().is_empty(), "{:?}", report.findings());
    let expression = report.binding(&RelativeBindingName::new("value")).unwrap();
    let expected = TypeExpr::Struct {
        fields: vec![
            ("label".into(), TypeExpr::String),
            (
                "inner".into(),
                TypeExpr::Struct {
                    fields: vec![("leaf".into(), TypeExpr::Int)],
                },
            ),
        ],
    };
    assert_eq!(
        admit_expression(expression, &expected),
        Ok(ExpressionAdmission::ValueCheck)
    );
    let error = check_type_expr_value_providerless(
        &expected,
        &parsed.variables["value"],
        &config,
        TypecheckInference::Judged(expression),
    )
    .require_provider_schemas()
    .unwrap_err()
    .to_string();
    assert!(error.contains("leaf") && error.contains("Int"), "{error}");
}

#[test]
fn dynamic_call_needs_an_export_annotation_without_poisoning_known_results() {
    let parsed = crate::parser::parse_and_resolve(
        "let value = lookup({ a = 'one' }, 'a', 'default')\nlet count = length([lookup({ a = 'one' }, 'a', 'default')])",
    ).unwrap();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config());
    assert!(report.findings().is_empty(), "{:?}", report.findings());
    let value = report.binding(&RelativeBindingName::new("value")).unwrap();
    assert!(matches!(
        value.static_type(),
        Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
    ));
    assert_eq!(
        admit_expression(value, &TypeExpr::String),
        Ok(ExpressionAdmission::ValueCheck)
    );
    assert_eq!(
        report
            .binding(&RelativeBindingName::new("count"))
            .unwrap()
            .static_type(),
        Ok(&TypeExpr::Int)
    );
}

#[test]
fn each_coalesce_still_rejects_a_noninferable_fallback_before_folding() {
    let parsed = crate::parser::parse_and_resolve(
        "let a = 'present'\nlet first = a ?? { label = 'x' inner = { leaf = 1 } }\nlet second = a ?? { label = 'x' inner = { leaf = 1 } }",
    ).unwrap();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config());
    assert_eq!(report.findings().len(), 2, "{:?}", report.findings());
    for name in ["first", "second"] {
        let expression = report.binding(&RelativeBindingName::new(name)).unwrap();
        assert!(admit_expression(expression, &TypeExpr::String).is_err());
    }
    assert_ne!(
        report.findings()[0].location(),
        report.findings()[1].location()
    );
}

#[test]
fn known_builtin_results_retain_invalid_operand_accesses() {
    let parsed = crate::parser::parse_and_resolve(
        "let source = mock.test.Source {}\nlet result = length([source.name[0]])",
    )
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let report = judge_expressions(&parsed, &schemas, &config());
    assert_eq!(report.findings().len(), 1, "{:?}", report.findings());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(matches!(
        result.static_type(),
        Err(InferenceIssue::Failed(InferenceError::InvalidAccess { .. }))
    ));
    assert!(admit_expression(result, &TypeExpr::Int).is_err());
}
