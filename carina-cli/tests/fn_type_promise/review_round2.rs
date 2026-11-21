use super::*;

const ROLE: &str = "let a = test.Role { role_name = 'dev' description = 'prod' }\n";
const PICK: &str = "fn pick(x: 'dev' | 'prod'): String { x }\n";
const PICK_ERROR: &str = "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.";

fn project(source: &str) -> Directory {
    let directory = Directory::new(Scope::Root);
    directory.write_source(source);
    directory
}

fn parity(directory: &Directory, expected: &[&str]) {
    let cli = validate(directory);
    let mut actual = messages(&cli);
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "{}", cli.json);
    let mut lsp: Vec<_> = lsp_errors(directory)
        .into_iter()
        .map(|error| error.message)
        .collect();
    lsp.sort_unstable();
    assert_eq!(lsp, expected, "CLI/LSP finding set differs");
}

#[test]
fn tp6_known_resource_inputs_use_rule1() {
    let directory = project(&format!(
        "{ROLE}{PICK}let description_sink = test.Sink {{ name = 'description' value = pick(a.description) }}\nlet role_sink = test.Sink {{ name = 'role' value = pick(a.role_name) }}\n"
    ));
    parity(&directory, &[]);
}

#[test]
fn tp6_invalid_resource_input_keeps_value_error() {
    let directory = project(&format!(
        "let a = test.Role {{ role_name = 'qa' }}\n{PICK}test.Sink {{ name = 'sink' value = pick(a.role_name) }}\n"
    ));
    parity(
        &directory,
        &["function 'pick': parameter 'x': expected 'dev' | 'prod', got string 'qa'"],
    );
}

#[test]
fn tp6_computed_named_return_known_value_uses_rule1() {
    let directory = project(
        "fn suffix(x: test.RoleName): test.RoleName { \"${x}-tail\" }\ntest.Sink { name = 'sink' value = suffix('abc') }\n",
    );
    parity(&directory, &[]);
}

#[test]
fn tp6_unused_computed_named_return_is_deferred() {
    let directory = project("fn up(x: test.RoleName): test.RoleName { lower(x) }\n");
    parity(&directory, &[]);
}

#[test]
fn tp6_computed_named_return_unknown_is_rule3() {
    let directory = project(&format!(
        "{ROLE}fn up(x: test.RoleName): test.RoleName {{ lower(x) }}\ntest.Sink {{ name = 'sink' value = up(a.computed_name) }}\n"
    ));
    parity(
        &directory,
        &[
            "function 'up': return value is declared as test.RoleName, but the body's type is String, so validate cannot prove it fits. Pass a value known at validate, or declare the return type as String and let the resource attribute that receives it check the value.",
        ],
    );
}

#[test]
fn tp6_computed_named_argument_unknown_is_rule3() {
    let directory = project(&format!(
        "{ROLE}fn rn(x: test.RoleName): test.RoleName {{ x }}\ntest.Sink {{ name = 'sink' value = rn(lower(a.computed_name)) }}\n"
    ));
    parity(
        &directory,
        &[
            "function 'rn': parameter 'x' is declared as test.RoleName, but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.",
        ],
    );
}

#[test]
fn tp6_computed_string_attribute_and_module_call_admission_is_unchanged() {
    let directory = project(&format!(
        "{ROLE}let child = use {{ source = './child' }}\nchild {{ role_name = lower(a.computed_name) }}\ntest.Role {{ role_name = lower(a.computed_name) }}\n"
    ));
    std::fs::create_dir(directory.root().join("child")).unwrap();
    std::fs::write(
        directory.root().join("child/main.crn"),
        "arguments { role_name: test.RoleName }\nattributes { name: test.RoleName = role_name }\n",
    )
    .unwrap();
    parity(&directory, &[]);
}

#[test]
fn tp6_mismatch_and_promise_are_both_reported() {
    let directory = project(&format!(
        "{ROLE}{PICK}fn bad(x: Int): Int {{ x }}\ntest.Sink {{ name = 'sink' value = pick(a.arn) number = bad(a.arn) }}\n"
    ));
    parity(
        &directory,
        &[
            PICK_ERROR,
            "function 'bad': parameter 'x': expected Int, got String",
        ],
    );
}

#[test]
fn tp6_module_argument_error_and_promise_are_both_reported() {
    let directory = project(&format!(
        "{ROLE}{PICK}let child = use {{ source = './child' }}\nchild {{ count = 'oops' }}\ntest.Sink {{ name = 'sink' value = pick(a.arn) }}\n"
    ));
    std::fs::create_dir(directory.root().join("child")).unwrap();
    std::fs::write(
        directory.root().join("child/main.crn"),
        "arguments { count: Int }\nattributes { count: Int = count }\n",
    )
    .unwrap();
    let cli = validate(&directory);
    let actual = messages(&cli);
    assert_eq!(actual.len(), 2, "{}", cli.json);
    assert!(actual.contains(&PICK_ERROR), "{actual:?}");
    assert!(
        actual
            .iter()
            .any(|error| error.contains("module call") && error.contains("argument 'count'")),
        "{actual:?}"
    );
    let mut actual = actual.to_vec();
    actual.sort_unstable();
    let mut lsp: Vec<_> = lsp_errors(&directory)
        .into_iter()
        .map(|error| error.message)
        .collect();
    lsp.sort_unstable();
    assert_eq!(lsp, actual);
}

#[test]
fn tp6_known_relevant_return_argument_uses_rule1() {
    let directory = project(&format!(
        "{ROLE}fn pick(x: String, y: String): 'dev' | 'prod' {{ x }}\ntest.Sink {{ name = 'sink' value = pick('dev', a.arn) }}\n"
    ));
    parity(&directory, &[]);
}

#[test]
fn tp6_invalid_known_relevant_return_keeps_value_error() {
    let directory = project(&format!(
        "{ROLE}fn pick(x: String, y: String): 'dev' | 'prod' {{ x }}\ntest.Sink {{ name = 'sink' value = pick('qa', a.arn) }}\n"
    ));
    parity(
        &directory,
        &["function 'pick': return value: expected 'dev' | 'prod', got string 'qa'"],
    );
}

#[test]
fn tp6_anonymous_refinement_advice_uses_authorable_types() {
    let directory = project(&format!(
        "{ROLE}{PICK}test.Sink {{ name = 'sink' value = pick(a.computed_description) }}\n"
    ));
    parity(
        &directory,
        &[
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String(pattern, len: ..=1000), so validate cannot prove it fits. Pass a value known at validate, or give the argument a type that proves it fits.",
        ],
    );
}
