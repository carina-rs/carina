//! HEAD's three failure modes and strict deferred-body checks are contracts.
use super::*;
use crate::builtins::BuiltinFunction;
use crate::resource::FunctionArgument;

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

fn parsed(expression: &str) -> ParsedFile {
    crate::parser::parse(
        &format!("let item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
        &context(),
    )
    .unwrap()
}

fn failed_call() -> Value {
    Value::Deferred(DeferredValue::FunctionCall {
        name: BuiltinFunction::length,
        args: vec![FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::Bool(true),
        ))],
        source_line: 1.into(),
    })
}

fn failed_user_call() -> ParsedFile {
    let mut file = crate::parser::parse(
        "fn fail(name: String): String { env(name) }\nlet item = mock.test.resource { name = 'item' }",
        &context(),
    )
    .unwrap();
    let functions = std::sync::Arc::new(file.user_functions.clone());
    let call = Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function: std::sync::Arc::new(file.user_functions["fail"].clone()),
        functions,
        args: vec![FunctionArgument::from_value(Value::Concrete(
            ConcreteValue::String("CARINA_3979_MISSING_USER_CALL".into()),
        ))],
        source_line: 2.into(),
    });
    file.resources[0].attributes.insert("comment".into(), call);
    file
}

#[test]
fn builtin_failure_modes_match_head() {
    for (resolution, accepted) in [
        (strict_resolution_context(), false),
        (analysis_resolution_context(), true),
        (diagnostics_resolution_context(), false),
        (strict_resolution_context().retained(), true),
    ] {
        for expression in ["length(true)", "secret(length(true))"] {
            let result = resolve_resource_refs_in_context(
                &mut parsed(expression),
                &context(),
                None,
                resolution,
            );
            assert_eq!(result.is_ok(), accepted, "{resolution:?}: {result:?}");
        }
    }
}

#[test]
fn failed_user_call_modes_match_head() {
    for (resolution, accepted) in [
        (strict_resolution_context(), false),
        (analysis_resolution_context(), false),
        (diagnostics_resolution_context(), true),
        (strict_resolution_context().retained(), true),
    ] {
        let mut file = failed_user_call();
        let result = resolve_resource_refs_in_context(&mut file, &context(), None, resolution);
        assert_eq!(result.is_ok(), accepted, "{resolution:?}: {result:?}");
        if accepted {
            assert!(matches!(
                file.resources[0].get_attr("comment"),
                Some(Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }))
            ));
        }
    }
    assert!(
        resolve_resource_refs_defer_unknown_functions(&mut failed_user_call(), &context()).is_err()
    );
    assert!(
        resolve_snapshot_function_calls_in_context(
            &mut failed_user_call(),
            &context(),
            analysis_resolution_context()
        )
        .is_err()
    );
    assert!(resolve_resource_refs_for_diagnostics(&mut failed_user_call(), &context()).is_ok());
}

#[test]
fn strict_entry_points_and_diagnostics_still_reject_builtin_failures() {
    assert!(resolve_resource_refs_with_config(&mut parsed("length(true)"), &context()).is_err());
    assert!(
        resolve_resource_refs_for_diagnostics(&mut parsed("length(true)"), &context()).is_err()
    );
    crate::validation::deferred_types::with_session_for_test(&context(), |session| {
        let file = parsed("length(true)");
        assert!(resolve_resource_refs_in_session(&mut file.clone(), session).is_err());
        let tree = super::super::AnalysisTree::try_from_file(&file).unwrap();
        assert!(resolve_resource_refs_from_analysis(&tree, session).is_err());
    });
    assert!(
        resolve_resource_refs_defer_unknown_functions(&mut parsed("length(true)"), &context())
            .is_ok()
    );
    assert!(
        resolve_snapshot_function_calls_in_context(
            &mut parsed("length(true)"),
            &context(),
            analysis_resolution_context()
        )
        .is_ok()
    );
}

#[test]
fn strict_deferred_value_if_checks_both_branches() {
    for expression in [
        "if flag { length(true) } else { 'ok' }",
        "if flag { 'ok' } else { length(true) }",
    ] {
        let mut file = crate::parser::parse(
            &format!("arguments {{ flag: Bool }}\nlet item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
            &context(),
        ).unwrap();
        assert!(matches!(
            file.resources[0].get_attr("comment"),
            Some(Value::Deferred(DeferredValue::ModuleIf(_)))
        ));
        let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
        assert!(error.to_string().contains("length"), "{error}");
    }
}

#[test]
fn strict_deferred_structural_if_checks_both_body_directives() {
    for branch_index in 0..2 {
        let mut file = crate::parser::parse(
            "arguments { flag: Bool }\nlet choice = if flag { mock.test.resource { name = 'a' directives { force_delete = true } } } else { mock.test.resource { name = 'b' directives { force_delete = true } } }",
            &context(),
        ).unwrap();
        let branch = file.deferred_module_ifs[0]
            .branches_mut()
            .nth(branch_index)
            .unwrap();
        let super::super::DeferredIfBody::Resource(resource) = branch else {
            panic!("resource branch")
        };
        resource.directives.force_delete.as_mut().unwrap().value = failed_call();
        let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
        assert!(error.to_string().contains("length"), "{error}");
    }
}

#[test]
fn strict_deferred_for_template_rejects_known_builtin_failure() {
    let mut file = crate::parser::parse(
        "let origin = mock.test.resource { name = 'origin' }\nfor item in origin.items { mock.test.resource { name = item comment = length(true) } }",
        &context(),
    ).unwrap();
    assert_eq!(file.parsed_deferred_for_expressions().len(), 1);
    let error = resolve_resource_refs_with_config(&mut file, &context()).unwrap_err();
    assert!(error.to_string().contains("length"), "{error}");
}

#[test]
fn strict_coalesce_default_preserves_failures_and_folds_successes() {
    for expression in [
        "origin.comment ?? length(true)",
        "\"${origin.comment ?? length(true)}\"",
    ] {
        let mut file = crate::parser::parse(
            &format!("let origin = mock.test.resource {{ name = 'origin' }}\nlet item = mock.test.resource {{ name = 'item' comment = {expression} }}"),
            &context(),
        ).unwrap();
        resolve_resource_refs_with_config(&mut file, &context()).unwrap();
    }
    let mut file = parsed("'unused'");
    file.resources[0].attributes.insert(
        "comment".into(),
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("origin", "comment", vec![])),
            default: Box::new(Value::Deferred(DeferredValue::FunctionCall {
                name: BuiltinFunction::length,
                args: vec![FunctionArgument::from_value(Value::Concrete(
                    ConcreteValue::String("ok".into()),
                ))],
                source_line: 1.into(),
            })),
        }),
    );
    resolve_resource_refs_with_config(&mut file, &context()).unwrap();
    let Some(Value::Deferred(DeferredValue::Coalesce { default, .. })) =
        file.resources[0].get_attr("comment")
    else {
        panic!("retained coalesce")
    };
    assert_eq!(default.as_ref(), &Value::Concrete(ConcreteValue::Int(2)));
}

#[test]
fn retained_snapshot_does_not_require_failed_user_calls() {
    let mut file = failed_user_call();
    resolve_snapshot_function_calls_in_context(
        &mut file,
        &context(),
        analysis_resolution_context().retained(),
    )
    .unwrap();
    assert!(matches!(
        file.resources[0].get_attr("comment"),
        Some(Value::Deferred(DeferredValue::ResolvedFunctionCall { .. }))
    ));
}
