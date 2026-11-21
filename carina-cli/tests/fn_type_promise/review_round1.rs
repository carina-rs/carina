use super::*;

fn mock_messages(body: &str) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        format!(
            "provider mock {{}}\nbackend local {{ path = 'carina.state.json' }}\nlet src = mock.test.resource {{ name = 'source' }}\n{body}"
        ),
    )
    .unwrap();
    let diagnostics = validate_diagnostics_with_factories(
        &ResolvedPath::new(directory.path()).unwrap(),
        Vec::new(),
        Default::default(),
    );
    messages(&diagnostics)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

const INNER_PROMISE: &str = "function 'inner': parameter 'y' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.";

#[test]
fn tp3_unused_definite_return_mismatches() {
    for (declaration, expected) in [
        (
            "fn f(a: Int): String { a }",
            "function 'f': return value: expected String, got Int",
        ),
        (
            "fn g(x: String): Int { x }",
            "function 'g': return value: expected Int, got String",
        ),
    ] {
        assert_eq!(mock_messages(declaration), [expected]);
    }
}

#[test]
fn tp3_nested_definite_mismatch_in_unused_outer() {
    assert_eq!(
        mock_messages(
            "fn inner(y: Int): String { 'ok' }\nfn outer(x: String): String { inner(x) }"
        ),
        ["function 'inner': parameter 'y': expected Int, got String"]
    );
}

#[test]
fn tp3_nested_promise_reused_at_unknown_outer_calls() {
    let errors = mock_messages(
        "fn inner(y: 'dev' | 'prod'): String { y }\nfn outer(x: String): String { inner(x) }\nlet known = mock.test.resource { name = outer('dev') }\nlet first = mock.test.resource { name = outer(src.computed) }\nlet second = mock.test.resource { name = outer(upper(src.computed)) }",
    );
    assert_eq!(errors, [INNER_PROMISE, INNER_PROMISE]);
}

#[test]
fn tp3_nested_promise_skips_unused_outer() {
    assert_eq!(
        mock_messages(
            "fn inner(y: 'dev' | 'prod'): String { y }\nfn outer(x: String): String { inner(x) }"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn tp3_nested_promise_checks_only_relevant_unknown_arguments() {
    let errors = mock_messages(
        "fn inner(y: 'dev' | 'prod'): String { y }\nfn outer(x: String, unused: String): String { inner(x) }\nlet known = mock.test.resource { name = outer('dev', src.computed) }\nlet unknown = mock.test.resource { name = outer(src.computed, 'known') }",
    );
    assert_eq!(errors, [INNER_PROMISE]);
}

#[test]
fn tp3_nested_promise_propagates_through_declared_bodies() {
    assert_eq!(
        mock_messages(
            "fn inner(y: 'dev' | 'prod'): String { y }\nfn middle(x: String): String { inner(x) }\nfn outer(x: String): String { middle(x) }\nmock.test.resource { name = outer(src.computed) }"
        ),
        [INNER_PROMISE]
    );
}

#[test]
fn tp3_plain_let_field_keeps_authored_type() {
    for expression in ["src.computed", "upper(src.computed)"] {
        assert_eq!(
            mock_messages(&format!(
                "let cfg = {{ env = {expression} }}\nfn f(x: String): String {{ x }}\nmock.test.resource {{ name = f(cfg.env) }}"
            )),
            Vec::<String>::new()
        );
    }
}

#[test]
fn tp3_missing_upstream_has_only_the_upstream_error() {
    for value in [
        "up.foo",
        "lookup({ env = up.foo }, 'env', 'dev')",
        "cfg.env",
    ] {
        let errors = mock_messages(&format!(
            "let up = upstream_state {{ source = '../nonexistent' }}\nlet cfg = {{ env = up.foo }}\nfn f(x: String): String {{ x }}\nmock.test.resource {{ name = f({value}) }}"
        ));
        assert_eq!(
            errors,
            ["upstream_state 'up': source '../nonexistent' does not exist"]
        );
    }
}

#[test]
fn tp3_promise_and_unknown_attribute_are_both_reported() {
    let errors = mock_messages(
        "fn inner(y: 'dev' | 'prod'): String { 'ok' }\nmock.test.resource { name = inner(src.computed) }\nmock.test.resource { name = 'other' bogus = 1 }",
    );
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors.iter().any(|message| message == INNER_PROMISE),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|message| message.contains("Unknown attribute 'bogus'")),
        "{errors:?}"
    );
}

#[test]
fn tp3_handle_parameter_is_proven() {
    assert_eq!(
        mock_messages(
            "fn h(r: mock.test.resource): String { r.computed }\nmock.test.resource { name = h(src) }"
        ),
        Vec::<String>::new()
    );
}

#[test]
fn tp3_definite_parameter_mismatch_keeps_main_text() {
    assert_eq!(
        mock_messages(
            "let role = mock.iam.Role { role_name = 'r' }\nfn f(x: String): String { x }\nmock.test.resource { name = f(role.max_session_duration) }"
        ),
        ["function 'f': parameter 'x': expected String, got Int"]
    );
}

#[test]
fn tp3_unknown_source_has_approved_advice() {
    assert_eq!(
        mock_messages(
            "fn f(x: String): String { x }\nmock.test.resource { name = f(lookup({ env = src.computed }, 'env', 'dev')) }"
        ),
        [
            "function 'f': parameter 'x' is declared as String, but its argument has no known type, so validate cannot prove it fits. Pass a value known at validate, or give the argument a type that proves it fits."
        ]
    );
}

#[test]
fn tp3_untyped_export_return_exact_text() {
    let directory = Directory::new(Scope::Root);
    directory.write_source("let orgs = upstream_state { source = '../upstream' }\nfn f(): String { orgs.untyped }\ntest.Sink { name = 'sink' value = f() }\n");
    assert_eq!(
        messages(&validate(&directory)),
        [
            "function 'f': return value is declared as String, but its value comes from upstream export 'orgs.untyped', which has no declared type, so validate cannot prove it fits. Pass a value known at validate, or declare the export's type."
        ]
    );
}
