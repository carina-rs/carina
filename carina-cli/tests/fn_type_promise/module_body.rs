use super::*;

fn module_argument(declaration: &str, argument: &str) -> Directory {
    let directory = Directory::new(Scope::Module);
    let upstream = if argument.starts_with("upstream.") {
        "let upstream = upstream_state { source = '../upstream' }\n"
    } else {
        ""
    };
    std::fs::write(
        directory.root().join("main.crn"),
        format!(
            "{SOURCE}{upstream}let child = use {{ source = './child' }}\nchild {{ env = {argument} }}\n"
        ),
    )
    .unwrap();
    directory.write_source(&format!(
        "arguments {{ {declaration} }}\nfn pick(x: {ENVIRONMENT}): String {{ 'accepted' }}\ntest.Sink {{ name = 'sink' value = pick(env) }}\n"
    ));
    directory
}

fn assert_child_call(diagnostics: &ValidateDiagnostics) {
    let location = &diagnostics.json["errors"][0]["location"];
    assert!(
        location["file"]
            .as_str()
            .is_some_and(|file| file.ends_with("child/main.crn")),
        "the finding belongs to the child's function call: {}",
        diagnostics.json
    );
    assert_eq!(location["range"]["start"]["line"], 3, "{location}");
}

#[test]
fn tp4_module_declared_union_proves_unknown_argument() {
    let directory = module_argument("env: 'dev' | 'prod'", "src.identifier");
    assert_accepted(&directory);
}

#[test]
fn tp4_module_declared_string_cannot_prove_unknown_argument() {
    let directory = module_argument("env: String", "src.identifier");
    let diagnostics = validate(&directory);
    assert_eq!(
        messages(&diagnostics),
        [
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value."
        ],
        "{}",
        diagnostics.json
    );
    assert_child_call(&diagnostics);
}

#[test]
fn tp4_module_known_argument_uses_value_rule() {
    let directory = module_argument("env: String", "'dev'");
    assert_accepted(&directory);
}

#[test]
fn tp4_module_known_invalid_argument_has_value_error() {
    let directory = module_argument("env: String", "'qa'");
    let diagnostics = validate(&directory);
    let errors = messages(&diagnostics);
    assert_eq!(errors.len(), 1, "{}", diagnostics.json);
    assert!(
        errors[0].contains("function 'pick': parameter 'x'"),
        "{errors:?}"
    );
    assert!(errors[0].contains("'dev' | 'prod'"), "{errors:?}");
    assert!(errors[0].contains("'qa'"), "{errors:?}");
    assert!(!errors[0].contains("cannot prove"), "{errors:?}");
    assert!(
        diagnostics.json["errors"][0]["location"]["file"]
            .as_str()
            .is_some_and(|file| file.ends_with("child/main.crn")),
        "{}",
        diagnostics.json
    );
}

#[test]
fn tp4_module_diagnostic_does_not_expose_callers_upstream_export() {
    let directory = module_argument("env: String", "upstream.untyped");
    let diagnostics = validate(&directory);
    assert_eq!(
        messages(&diagnostics),
        [
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value."
        ],
        "{}",
        diagnostics.json
    );
    assert_child_call(&diagnostics);
}

#[test]
fn tp4_module_argument_alias_keeps_declared_type() {
    let directory = module_argument("env: 'dev' | 'prod'", "src.identifier");
    directory.write_source(&format!(
        "arguments {{ env: {ENVIRONMENT} }}\nfn pick(x: {ENVIRONMENT}): String {{ 'accepted' }}\nlet alias = env\ntest.Sink {{ name = 'sink' value = pick(alias) }}\n"
    ));
    assert_accepted(&directory);
}

#[test]
fn tp4_module_unavailable_schema_defers_promise() {
    for argument in ["src.identifier", "upper(src.identifier)"] {
        let directory = module_argument("env: String", argument);
        let diagnostics = validate_diagnostics_with_factories(
            &ResolvedPath::new(directory.root()).unwrap(),
            Vec::new(),
            std::collections::HashMap::from([("test".into(), "provider not installed".into())]),
        );
        assert!(
            messages(&diagnostics)
                .iter()
                .all(|error| !error.contains("function 'pick'")),
            "unavailable schemas must leave promise checks pending: {}",
            diagnostics.json
        );
        assert!(
            diagnostics
                .json
                .to_string()
                .contains("provider not installed")
        );
    }
}
