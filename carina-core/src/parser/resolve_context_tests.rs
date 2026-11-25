//! HEAD's three failure modes and strict deferred-body checks are contracts.
use super::*;
use crate::builtins::BuiltinFunction;
use crate::parser::ast::with_user_function_binding_map_count;
use crate::resource::{FunctionArgument, same_plain_representation};

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

#[derive(Clone, Copy, Debug)]
enum ShareFnShape {
    Coalesce,
    Conditional,
    Interpolation,
    Join,
}

fn share_fn_definitions(depth: usize, shape: ShareFnShape) -> Vec<String> {
    let mut definitions = vec!["fn f0(x: String): String { x }".to_string()];
    for level in 1..=depth {
        let call = format!("f{}(x)", level - 1);
        let body = match shape {
            ShareFnShape::Coalesce => format!("{call} ?? {call}"),
            ShareFnShape::Conditional => format!("if true {{ {call} }} else {{ {call} }}"),
            ShareFnShape::Interpolation => format!("\"${{{call}}}${{{call}}}\""),
            ShareFnShape::Join => format!("join('', [{call}, {call}])"),
        };
        definitions.push(format!("fn f{level}(x: String): String {{ {body} }}"));
    }
    definitions
}

fn share_fn_counts(shape: ShareFnShape, depth: usize, directory: bool) -> (usize, usize) {
    let definitions = share_fn_definitions(depth, shape).join("\n");
    let consumer = format!("let v = f{depth}('a')\n");
    let (((), mappings), walks) = crate::value_walk::with_function_definition_walk_count(|| {
        with_user_function_binding_map_count(|| {
            let parsed = if directory {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("a_functions.crn"), &definitions).unwrap();
                std::fs::write(dir.path().join("main.crn"), &consumer).unwrap();
                crate::config_loader::parse_directory(
                    &crate::paths::ResolvedPath::new(dir.path()).unwrap(),
                    &context(),
                )
                .unwrap()
                .into_parsed()
            } else {
                crate::parser::parse(&format!("{definitions}\n{consumer}"), &context()).unwrap()
            };
            let result = &parsed.variables["v"];
            if matches!(shape, ShareFnShape::Interpolation | ShareFnShape::Join) {
                assert!(
                    matches!(result, Value::Concrete(ConcreteValue::String(value))
                    if value.expose_plaintext() == "a".repeat(1 << depth))
                );
            }
            let _ = parsed.resolve_root_for_test();
        })
    });
    (mappings, walks)
}

fn share_fn_assert_polynomial(shape: ShareFnShape, directory: bool) {
    let mut previous = None;
    for depth in [2, 4, 8, 12] {
        let counts = share_fn_counts(shape, depth, directory);
        eprintln!(
            "share_fn_counts shape={shape:?} directory={directory} depth={depth} binding={} transform={}",
            counts.0, counts.1
        );
        if let Some((old_depth, (bindings, transforms))) = previous {
            // Doubling depth allows fourth-degree growth with headroom. Repeated
            // walks per call occurrence grow exponentially even for these DAGs.
            if depth == 2 * old_depth {
                assert!(
                    counts.0 <= bindings * 16,
                    "{shape:?} binding mappings must grow polynomially: depth {old_depth}: {bindings}, depth {depth}: {}",
                    counts.0
                );
                assert!(
                    counts.1 <= transforms * 16,
                    "{shape:?} transform definition walks must grow polynomially: depth {old_depth}: {transforms}, depth {depth}: {}",
                    counts.1
                );
            }
        }
        assert!(
            counts.0 < 1_000_000 && counts.1 < 1_000_000,
            "{shape:?} definition walks at depth {depth} must stay bounded: {counts:?}"
        );
        previous = Some((depth, counts));
    }
}

#[test]
fn share_fn_coalesce_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Coalesce, false);
}

#[test]
fn share_fn_conditional_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Conditional, false);
}

#[test]
fn share_fn_interpolation_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Interpolation, false);
}

#[test]
fn share_fn_join_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Join, false);
}

#[test]
fn share_fn_directory_sibling_definition_walks_grow_polynomially() {
    share_fn_assert_polynomial(ShareFnShape::Interpolation, true);
}

#[test]
fn share_fn_forward_file_root_consumer_before_declaration_remains_a_call() {
    let parsed = crate::parser::parse(
        r#"let v = g("a")
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
"#,
        &context(),
    )
    .unwrap();

    // A seeded function is not evaluated, even with a static argument.
    assert!(matches!(
        &parsed.variables["v"],
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, source_line, .. })
            if function.name == "g"
                && source_line.get() == Some(1)
                && args.len() == 1
                && matches!(args[0].value(), Some(Value::Concrete(ConcreteValue::String(s))) if s == "a")
    ));

    // Root binding resolution changes addresses; it does not evaluate the call.
    let resolved = parsed.resolve_root_for_test();
    assert!(matches!(
        &resolved.variables["v"],
        GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { function, args, source_line, .. })
            if function.name == "g"
                && source_line.get() == Some(1)
                && args.len() == 1
                && matches!(args[0].value(), Some(GenericValue::Concrete(ConcreteValue::String(s))) if s == "a")
    ));
}

#[test]
fn share_fn_forward_file_root_consumer_after_declaration_evaluates_to_interpolation() {
    let parsed = crate::parser::parse(
        r#"let s = "outer"
fn g(x: String): String { "${x}-${s}" }
let v = g("a")
"#,
        &context(),
    )
    .unwrap();

    // Function bodies do not capture root lets. The unbound s is an enum
    // identifier, which interpolation canonicalization does not stringify.
    assert!(matches!(
        &parsed.variables["v"],
        Value::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));

    let resolved = parsed.resolve_root_for_test();
    assert!(matches!(
        &resolved.variables["v"],
        GenericValue::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(GenericValue::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));
}

#[test]
fn share_fn_forward_file_deferred_consumers_capture_bootstrap_or_final_body() {
    for (before_declaration, source) in [
        (
            true,
            r#"arguments { input: String }
let v = g(input)
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
"#,
        ),
        (
            false,
            r#"arguments { input: String }
let s = "outer"
fn g(x: String): String { "${x}-${s}" }
let v = g(input)
"#,
        ),
    ] {
        let parsed = crate::parser::parse(source, &context()).unwrap();
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &parsed.variables["v"]
        else {
            panic!("g(input) must remain a deferred user-function call")
        };
        assert_eq!(function.name, "g");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert!(matches!(
            &function.body.value,
            Value::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Expr(Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::FnParam { name },
                        ))),
                        InterpolationPart::Literal(separator),
                        InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if name == "x" && separator == "-" && s.as_str() == "s"
                )
        ));

        let final_body = &parsed.user_functions["g"].body;
        assert!(same_plain_representation(
            &function.body.value,
            &final_body.value
        ));
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert_eq!(
            function.body.source_file.as_deref(),
            before_declaration.then_some(std::path::Path::new("test.crn"))
        );
        assert!(final_body.source_file.is_none());
        // These are all four body fields: only source_file differs for a
        // forward call. An after-declaration call has the final body.
        assert_eq!(
            function.body.source_file == final_body.source_file,
            !before_declaration
        );
        assert_eq!(
            functions.keys().map(String::as_str).collect::<Vec<_>>(),
            ["g"]
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);

        let resolved = parsed.resolve_root_for_test();
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &resolved.variables["v"]
        else {
            panic!("root binding resolution must retain g(input)")
        };
        let final_body = &resolved.user_functions["g"].body;
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert_eq!(function.body.value, final_body.value);
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert_eq!(
            function.body.source_file.as_deref(),
            before_declaration.then_some(std::path::Path::new("test.crn"))
        );
        assert!(final_body.source_file.is_none());
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }
}

#[test]
fn share_fn_forward_file_function_body_captures_bootstrap_callee_and_scope() {
    let parsed = crate::parser::parse(
        r#"let s = "outer"
fn outer(x: String): String { inner(x) }
fn inner(x: String): String { "${x}-${s}" }
"#,
        &context(),
    )
    .unwrap();
    let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        args,
        ..
    }) = &parsed.user_functions["outer"].body.value
    else {
        panic!("outer must retain its forward call to inner")
    };
    assert_eq!(function.name, "inner");
    assert_eq!(args.len(), 1);
    assert!(matches!(
        args[0].value(),
        Some(Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam { name }))) if name == "x"
    ));
    let final_body = &parsed.user_functions["inner"].body;
    assert!(same_plain_representation(
        &function.body.value,
        &final_body.value
    ));
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert_eq!(
        function.body.source_file.as_deref(),
        Some(std::path::Path::new("test.crn"))
    );
    assert!(final_body.source_file.is_none());
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["outer", "inner"]
    );
    // The captured outer is itself the prepass copy: its inner call was
    // parsed when only inner had been added to the bootstrap table.
    let Value::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("bootstrap outer must contain a resolved inner call")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );

    let resolved = parsed.resolve_root_for_test();
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &resolved.user_functions["outer"].body.value
    else {
        panic!("root binding resolution must retain outer's call")
    };
    let final_body = &resolved.user_functions["inner"].body;
    assert_eq!(function.body.value, final_body.value);
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert_eq!(
        function.body.source_file.as_deref(),
        Some(std::path::Path::new("test.crn"))
    );
    assert!(final_body.source_file.is_none());
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("root binding resolution must retain bootstrap outer's scope")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );
}

#[test]
fn share_fn_forward_directory_root_consumers_before_and_after_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let main_path = directory.path().join("main.crn");
    let helpers_path = directory.path().join("z_helpers.crn");
    let main = "let v_before = g(\"a\")\nlet s = \"outer\"\n";
    let helpers = "fn g(x: String): String { \"${x}-${s}\" }\nlet v_after = g(\"a\")\n";
    std::fs::write(&main_path, main).unwrap();
    std::fs::write(&helpers_path, helpers).unwrap();

    // Inspect the per-file parser output before the directory loader's
    // separate function-evaluation pass removes the forward static call.
    let files = crate::config_loader::parse_directory_files(
        &[(main_path, main.into()), (helpers_path, helpers.into())],
        &context(),
    )
    .unwrap();
    assert!(matches!(
        &files[0].1.parsed().variables["v_before"],
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. })
            if function.name == "g" && args.len() == 1
                && matches!(args[0].value(), Some(Value::Concrete(ConcreteValue::String(s))) if s == "a")
    ));
    assert!(matches!(
        &files[1].1.parsed().variables["v_after"],
        Value::Deferred(DeferredValue::Interpolation(parts))
            if matches!(
                parts.as_slice(),
                [
                    InterpolationPart::Literal(prefix),
                    InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                ] if prefix == "a-" && s.as_str() == "s"
            )
    ));

    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();
    // To confirm by running: the loader's evaluation pass retains the enum
    // operand instead of folding either result to a concrete string.
    for name in ["v_before", "v_after"] {
        assert!(matches!(
            &parsed.variables[name],
            Value::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Literal(prefix),
                        InterpolationPart::Expr(Value::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if prefix == "a-" && s.as_str() == "s"
                )
        ));
    }

    let resolved = parsed.resolve_root_for_test();
    for name in ["v_before", "v_after"] {
        assert!(matches!(
            &resolved.variables[name],
            GenericValue::Deferred(DeferredValue::Interpolation(parts))
                if matches!(
                    parts.as_slice(),
                    [
                        InterpolationPart::Literal(prefix),
                        InterpolationPart::Expr(GenericValue::Concrete(ConcreteValue::EnumIdentifier(s))),
                    ] if prefix == "a-" && s.as_str() == "s"
                )
        ));
    }
}

#[test]
fn share_fn_forward_directory_deferred_consumers_capture_bootstrap_or_local_body() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "arguments { input: String }\nlet v_before = g(input)\nlet s = \"outer\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("z_helpers.crn"),
        "fn g(x: String): String { \"${x}-${s}\" }\nlet v_after = g(input)\n",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();

    for name in ["v_before", "v_after"] {
        let Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &parsed.variables[name]
        else {
            panic!("{name} must remain a deferred g(input) call")
        };
        assert_eq!(function.name, "g");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        let final_body = &parsed.user_functions["g"].body;
        assert!(same_plain_representation(
            &function.body.value,
            &final_body.value
        ));
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert!(final_body.source_file.is_some());
        // To confirm by running: directory metadata stamps the stored final
        // definition after calls have captured it. The bootstrap body was
        // already stamped, so it equals the final body; the local copy lacks it.
        assert_eq!(function.body.source_file.is_some(), name == "v_before");
        assert_eq!(
            function.body.source_file == final_body.source_file,
            name == "v_before"
        );
        assert_eq!(
            functions.keys().map(String::as_str).collect::<Vec<_>>(),
            ["g"]
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }

    let resolved = parsed.resolve_root_for_test();
    for name in ["v_before", "v_after"] {
        let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            ..
        }) = &resolved.variables[name]
        else {
            panic!("root binding resolution must retain {name}")
        };
        let final_body = &resolved.user_functions["g"].body;
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].pending_parameter(), Some("input"));
        assert_eq!(function.body.value, final_body.value);
        assert_eq!(function.body.location, final_body.location);
        assert_eq!(function.body.source_line, final_body.source_line);
        assert!(final_body.source_file.is_some());
        assert_eq!(function.body.source_file.is_some(), name == "v_before");
        assert_eq!(
            function.body.source_file == final_body.source_file,
            name == "v_before"
        );
        assert_eq!(functions["g"].body.source_file, function.body.source_file);
    }
}

#[test]
fn share_fn_forward_directory_function_body_captures_bootstrap_callee_and_scope() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "arguments { input: String }\nlet v = outer(input)\nlet s = \"outer\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("z_helpers.crn"),
        "fn outer(x: String): String { inner(x) }\nfn inner(x: String): String { \"${x}-${s}\" }\n",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed();
    let Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        args,
        ..
    }) = &parsed.user_functions["outer"].body.value
    else {
        panic!("outer must retain its forward call to inner")
    };
    assert_eq!(function.name, "inner");
    assert_eq!(args.len(), 1);
    assert!(matches!(
        args[0].value(),
        Some(Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::FnParam { name }))) if name == "x"
    ));
    let final_body = &parsed.user_functions["inner"].body;
    assert!(same_plain_representation(
        &function.body.value,
        &final_body.value
    ));
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert!(final_body.source_file.is_some());
    assert_eq!(function.body.source_file, final_body.source_file);
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["outer", "inner"]
    );
    assert!(!same_plain_representation(
        &functions["outer"].body.value,
        &parsed.user_functions["outer"].body.value
    ));
    let Value::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("bootstrap outer must retain the earlier function table")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );

    let resolved = parsed.resolve_root_for_test();
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions,
        ..
    }) = &resolved.user_functions["outer"].body.value
    else {
        panic!("root binding resolution must retain outer's call")
    };
    let final_body = &resolved.user_functions["inner"].body;
    assert_eq!(function.body.value, final_body.value);
    assert_eq!(function.body.location, final_body.location);
    assert_eq!(function.body.source_line, final_body.source_line);
    assert!(final_body.source_file.is_some());
    assert_eq!(function.body.source_file, final_body.source_file);
    let GenericValue::Deferred(DeferredValue::ResolvedFunctionCall { functions, .. }) =
        &functions["outer"].body.value
    else {
        panic!("root binding resolution must retain bootstrap outer's scope")
    };
    assert_eq!(
        functions.keys().map(String::as_str).collect::<Vec<_>>(),
        ["inner"]
    );
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
