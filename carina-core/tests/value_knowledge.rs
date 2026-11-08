//! Public proof contracts and operational regressions for carina#4316.

use std::convert::Infallible;

use carina_core::resource::knowledge::{
    Classification, EvaluationKind, KnowledgeContext, KnowledgeError, Phase, PlanResolved,
    SubstitutionKind, WaitOn,
};
use carina_core::resource::{
    ApplyTimeUnknown, ConcreteValue, DeferredValue, InterpolationPart, LoopSlot, NoPlaceholder,
    ParserPlaceholder, PendingReRead, RelativeBindingName, Value,
};
use indexmap::IndexMap;

type V = Value<NoPlaceholder, RelativeBindingName>;

fn secret(value: V) -> V {
    Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
}

fn assert_apply_operand_timing(binding: &str, attribute: &str) {
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-cli/tests/fixtures/value_knowledge/apply_operand_timing");
    let parsed = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(directory).unwrap(),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .into_parsed();
    let resource = parsed
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|name| name.as_str() == binding)
        })
        .unwrap();
    let value = resource.get_attr(attribute).unwrap();
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let knowledge = value.knowledge(&KnowledgeContext::plan(&bindings)).unwrap();
    let Classification::Pending(operation) = &*knowledge else {
        panic!("retained operation");
    };
    assert_eq!(operation.phase(), Phase::Apply);
    assert!(
        knowledge.sites().all(|site| site.phase() == Phase::Apply),
        "{binding} retained a Plan site: {:?}",
        knowledge.sites().collect::<Vec<_>>()
    );
    assert!(PlanResolved::try_from(knowledge).is_ok());
}

#[test]
fn apply_operand_user_call_has_no_plan_sites() {
    assert_apply_operand_timing("direct", "present");
}

#[test]
fn apply_operand_builtin_has_no_plan_sites() {
    assert_apply_operand_timing("builtin", "present");
}

#[test]
fn apply_operand_interpolation_has_no_plan_sites() {
    assert_apply_operand_timing("interpolation", "present");
}

#[test]
fn apply_operand_arithmetic_has_no_plan_sites() {
    assert_apply_operand_timing("arithmetic", "create_read_back_pending_polls");
}

#[test]
fn apply_operand_access_has_no_plan_sites() {
    assert_apply_operand_timing("access", "present");
}

#[test]
fn apply_operand_coalesce_has_no_plan_sites() {
    assert_apply_operand_timing("coalesce", "present");
}

#[test]
fn source_free_evaluation_has_a_nonempty_plan_site() {
    let value: V = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Literal("ready operands".into()),
    ]));
    let Classification::Pending(pending) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("an unevaluated expression cannot mint KnownValue");
    };
    assert_eq!(pending.phase(), Phase::Plan);
    assert_eq!(pending.sites().count(), 1);
    assert!(
        pending
            .sites()
            .next()
            .unwrap()
            .inspect::<bool, Infallible>(|path, operands, wait| Ok(path.segments().is_empty()
                && operands.is_empty()
                && matches!(wait, WaitOn::Evaluation(EvaluationKind::Interpolation))))
            .unwrap()
    );
}

#[test]
fn plan_projection_rejects_a_remaining_plan_evaluation() {
    let value: V = Value::Deferred(DeferredValue::Interpolation(vec![]));
    let _knowledge = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(matches!(
        PlanResolved::try_from(
            value
                .knowledge(&carina_core::resource::knowledge::KnowledgeContext::plan(
                    &carina_core::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        ),
        Err(KnowledgeError::UnresolvedAtBoundary { .. })
    ));
}

#[test]
fn plan_projection_accepts_apply_waits_but_rejects_plan_sites_inside_shapes() {
    let unknown: V = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
        PendingReRead::not_found(),
    )));
    let resolved = PlanResolved::try_from(
        unknown
            .knowledge(&KnowledgeContext::plan(
                &carina_core::binding_index::ResolvedBindings::default(),
            ))
            .unwrap(),
    )
    .unwrap();
    let Classification::Pending(pending) = resolved.knowledge() else {
        panic!("read-back still needs apply");
    };
    assert_eq!(pending.sites().next().unwrap().phase(), Phase::Apply);

    let list: V = Value::Concrete(ConcreteValue::List(vec![
        unknown,
        Value::Deferred(DeferredValue::Interpolation(vec![])),
    ]));
    assert!(matches!(
        PlanResolved::try_from(
            list.knowledge(&KnowledgeContext::plan(
                &carina_core::binding_index::ResolvedBindings::default()
            ))
            .unwrap()
        ),
        Err(KnowledgeError::UnresolvedAtBoundary { .. })
    ));
}

#[test]
fn parser_and_loop_substitutions_use_closed_kinds_without_locations() {
    let cases = [
        (
            ParserPlaceholder::FnParam { name: "x".into() },
            SubstitutionKind::FnParam,
        ),
        (
            ParserPlaceholder::FnLocal { name: "x".into() },
            SubstitutionKind::FnLocal,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Key),
            SubstitutionKind::LoopKey,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Index),
            SubstitutionKind::LoopIndex,
        ),
        (
            ParserPlaceholder::Loop(LoopSlot::Value),
            SubstitutionKind::LoopValue,
        ),
    ];
    for (slot, expected) in cases {
        let value: Value<ParserPlaceholder, RelativeBindingName> =
            Value::Deferred(DeferredValue::Placeholder(slot));
        let Classification::Pending(pending) = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap()
        else {
            panic!("unsubstituted slot");
        };
        assert!(
            pending
                .sites()
                .next()
                .unwrap()
                .inspect::<bool, Infallible>(|_, _, wait| {
                    Ok(matches!(wait, WaitOn::Substitution(kind) if *kind == expected))
                })
                .unwrap()
        );
    }
}

#[test]
fn partial_coalesce_propagates_shape_without_folding_authored_operands() {
    let left = V::Concrete(ConcreteValue::List(vec![V::resource_ref(
        "producer",
        "identifier",
        vec![],
    )]));
    let value = V::Deferred(DeferredValue::Coalesce {
        value: Box::new(left),
        default: Box::new(V::Concrete(ConcreteValue::List(vec![]))),
    });
    let Classification::ShapeKnown(shape) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("the existing left collection establishes membership");
    };
    assert_eq!(shape.len(), 1);
    assert_eq!(shape.to_value(), value);
}

#[test]
fn coalesce_requires_present_known_proof_and_never_forces_unused_fallback() {
    use carina_core::resource::knowledge::take_present;
    let value: V = Value::Concrete(ConcreteValue::Int(7));
    let Classification::Known(known) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("known")
    };
    assert_eq!(
        Ok::<_, carina_core::resolver::ResolutionError>(take_present(known)),
        Ok(value)
    );
}

#[test]
fn actual_scalar_wire_and_builtin_consumers_accept_known_proofs() {
    let value: Value = Value::Concrete(ConcreteValue::String("abc".into()));
    let Classification::Known(known) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("known string")
    };
    let wire = carina_core::protocol_convert::try_core_to_protocol_value(&known).unwrap();
    assert_eq!(wire, carina_provider_protocol::Value::String("abc".into()));
    let result = carina_core::builtins::evaluate_builtin_to_value(
        "upper",
        &[known],
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert_eq!(result, Value::Concrete(ConcreteValue::String("ABC".into())));
}

#[test]
fn known_secret_import_keeps_its_wrapper_and_is_not_an_ordinary_wire_value() {
    let value: Value = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::String(
            "import-secret-4316".into(),
        )))
        .into(),
    ));
    let Classification::Known(known) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("known protected string")
    };
    let selected = carina_core::effect::resolve_import_identifier(&known).unwrap();
    assert_eq!(selected.to_value(), value);
    let error = carina_core::protocol_convert::try_core_to_protocol_value(&known).unwrap_err();
    assert!(!format!("{error:?}").contains("import-secret-4316"));
}

#[test]
fn shape_only_builtin_can_count_pending_leaves_and_supply_known_map_keys() {
    use carina_core::parser::{ProviderContext, parse_file};
    let parsed = parse_file(
        r#"
let source = mock.test.resource { name = "source" }
let sink = mock.test.resource { count = length([source.identifier, source.identifier]) }
for key in keys({a = source.identifier, b = source.identifier}) {
  mock.test.resource { name = key }
}
"#,
        std::path::Path::new("knowledge.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert_eq!(parsed.resources.len(), 4);
    assert_eq!(
        parsed.resources[1]
            .get_attr("count")
            .and_then(
                |value| carina_core::parser::evaluate_static_value_for_validation(
                    &value
                        .knowledge(&KnowledgeContext::validation())
                        .map(|proof| proof.into_classification())
                        .unwrap(),
                    &ProviderContext::new(carina_core::EnvironmentSnapshot::empty())
                )
            )
            .as_ref(),
        Some(&Value::Concrete(ConcreteValue::Int(2)))
    );
}

#[test]
fn lexical_body_dependencies_are_part_of_user_call_knowledge() {
    use std::{collections::HashMap, sync::Arc};
    let parsed = carina_core::parser::parse_file(
        "fn captured() { 1 }",
        std::path::Path::new("functions.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let mut function = parsed.user_functions["captured"].clone();
    function.body.value = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
        PendingReRead::not_found(),
    )));
    let call: V = Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function: Arc::new(function),
        functions: Arc::new(HashMap::new()),
        args: vec![],
        source_line: 0.into(),
    });
    let Classification::Pending(pending) = call
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("user call")
    };
    assert_eq!(pending.phase(), Phase::Apply);
    assert!(pending.sites().any(|site| {
        site.inspect::<bool, Infallible>(|_, steps, wait| {
            Ok(
                steps.contains(&carina_core::resource::knowledge::OperandStep::FunctionBody)
                    && matches!(wait, WaitOn::Unknown(_)),
            )
        })
        .unwrap()
    }));
}

#[test]
fn lazy_plan_expression_waits_for_its_apply_discriminator() {
    let value: V = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::PendingReRead(PendingReRead::not_found()),
        ))),
        default: Box::new(Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("lazy".into()),
        ]))),
    });

    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&carina_core::resource::knowledge::KnowledgeContext::plan(
                    &carina_core::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok(),
        "the fallback cannot be evaluated before the discriminator"
    );
}

#[test]
fn flatten_known_inner_shapes_preserves_pending_leaves_for_real_parser_expansion() {
    let parsed = carina_core::parser::parse_file(
        r#"
let producer_a = mock.test.resource { name = "a" }
let producer_b = mock.test.resource { name = "b" }
for (i, item) in flatten([[producer_a.identifier], [producer_b.identifier]]) {
  mock.test.resource { name = "child-${i}" parent_name = item }
}
"#,
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert_eq!(parsed.resources.len(), 4);
}

#[test]
fn module_iteration_reports_apply_shape_with_protected_nested_paths() {
    let value: V = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "items.with.dots".into(),
        Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
            PendingReRead::not_found(),
        ))),
    )])));
    let Classification::ShapeKnown(shape) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("shape")
    };
    // The outer map can be enumerated even though its leaf cannot.
    assert!(
        carina_core::validation::validate_module_iteration_shape(&Classification::ShapeKnown(
            shape
        ))
        .is_ok()
    );
    let unknown: V = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
        PendingReRead::not_found(),
    )));
    let error = carina_core::validation::validate_module_iteration_shape(
        &unknown
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        error.required(),
        carina_core::resource::knowledge::Requirement::ShapeForPlan
    );
    assert_eq!(error.phase(), Phase::Apply);
    assert!(error.source().is_none());
    let protected = secret(unknown);
    let error = carina_core::validation::validate_module_iteration_shape(
        &protected
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap(),
    )
    .unwrap_err();
    assert!(!format!("{error:?}").contains("items.with.dots"));
}

#[test]
fn known_custom_type_inputs_report_kind_errors_with_protected_diagnostics() {
    use carina_core::{
        parser::{ProviderContext, validate_custom_type},
        schema::TypeIdentity,
    };
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let value: V = Value::Concrete(ConcreteValue::List(vec![]));
    let known = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
        .into_known()
        .unwrap();
    let error =
        validate_custom_type(&TypeIdentity::bare("PositiveInt"), &known, &config).unwrap_err();
    assert!(error.to_string().contains("Type mismatch"), "{error}");
    let protected = secret(V::Concrete(ConcreteValue::String("not-an-integer".into())));
    let known = protected
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
        .into_known()
        .unwrap();
    let error =
        validate_custom_type(&TypeIdentity::bare("PositiveInt"), &known, &config).unwrap_err();
    assert!(!error.to_string().contains("not-an-integer"));
}

mod collection_presence {
    use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    fn parse_fixture(name: &str) -> carina_core::parser::ParsedFile {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-cli/tests/fixtures/value_knowledge")
            .join(name);
        carina_core::config_loader::parse_directory(
            &carina_core::paths::ResolvedPath::new(directory).unwrap(),
            &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        )
        .unwrap()
        .into_parsed()
    }
    #[test]
    fn present_partial_coalesce_returns_the_original_left_list() {
        let parsed = parse_fixture("present_partial_coalesce");
        assert!(
            matches!(&parsed.variables["items"], Value::Concrete(ConcreteValue::List(items)) if items.len()==1 && matches!(items[0],Value::Deferred(DeferredValue::ResourceRef { .. })))
        );
    }
    #[test]
    fn authored_flatten_call_keeps_its_source_line() {
        let parsed = parse_fixture("flatten_location");
        assert!(
            matches!(&parsed.variables["items"], Value::Deferred(DeferredValue::FunctionCall { source_line, .. }) if source_line.get()==Some(3))
        );
    }
}

mod condition_timing {
    fn parse(name: &str) -> Result<carina_core::parser::ParsedFile, String> {
        let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-cli/tests/fixtures/value_knowledge")
            .join(name);
        carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
            &carina_core::paths::ResolvedPath::new(directory).unwrap(),
            &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
            &std::collections::HashMap::new(),
        )
        .map(|parsed| parsed.parsed.into_parsed())
        .map_err(|error| {
            error
                .parse_error()
                .map(ToString::to_string)
                .unwrap_or_else(|| error.to_string())
        })
    }
    #[test]
    fn secret_condition_keeps_runtime_diagnostic() {
        let error = parse("if_secret_runtime").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("if condition depends on a runtime value"),
            "{error}"
        );
    }
    #[test]
    fn immediate_env_failure_keeps_main_evaluation_diagnostic() {
        let error = parse("if_unavailable_runtime").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Invalid expression at line 0: for iterable function call 'env' failed: environment variable 'CARINA_KNOWLEDGE_CONDITION_MISSING' is not set"
        );
    }

    #[test]
    fn shape_only_length_condition_is_static() {
        parse("if_shape_length").unwrap();
    }
}

mod structural_consumers {
    use std::collections::HashMap;

    use carina_core::parser::ProviderContext;
    use carina_core::resource::knowledge::KnowledgeContext;
    use carina_core::resource::{
        ConcreteValue, DeferredValue, NoPlaceholder, RelativeBindingName, Value,
    };
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaKind, SchemaRegistry, ValueStructure,
    };

    #[test]
    fn classified_report_retains_coalesced_partial_collection_structure() {
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("Widget")
                .attribute(
                    AttributeSchema::optional(
                        "values",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let schema = registry
            .get("test", "Widget", SchemaKind::Resource)
            .unwrap();
        let value: Value<NoPlaceholder, RelativeBindingName> =
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Concrete(ConcreteValue::List(vec![
                    Value::resource_ref("producer", "identifier", vec![]),
                ]))),
                default: Box::new(Value::Concrete(ConcreteValue::List(vec![]))),
            });
        let attributes = HashMap::from([(
            "values".to_owned(),
            value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap(),
        )]);
        let context = ProviderContext::new(carina_core::EnvironmentSnapshot::new([]));
        let mut report = schema.validate_values_with_resolver(
            &attributes,
            &|_| false,
            context.custom_type_resolver(),
        );
        assert_eq!(report.errors().count(), 0);
        assert_eq!(
            report.take_attribute("values").unwrap().structure(),
            ValueStructure::List
        );
    }
}
