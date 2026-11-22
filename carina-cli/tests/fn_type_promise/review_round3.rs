use super::*;

fn project(source: &str) -> Directory {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!("{SOURCE}{source}"));
    directory
}

fn parity(directory: &Directory, expected: &[&str]) {
    let diagnostics = validate(directory);
    assert_eq!(messages(&diagnostics), expected, "{}", diagnostics.json);
    let actual: Vec<_> = lsp_errors(directory)
        .into_iter()
        .map(|d| d.message)
        .collect();
    assert_eq!(actual, expected, "CLI/LSP finding set differs");
}

#[test]
fn tp9_b1_struct_arguments_keep_main_admission() {
    parity(
        &project(
            "fn s(x: struct { name: String }): String { x.name }\nfn s2(x: struct { name: String, n: Int }): String { x.name }\nlet known = s({ name = 'abc' })\nlet unknown = s({ name = src.identifier })\nlet pair = s2({ name = src.identifier, n = src.count })\nlet mixed = s2({ name = 'x', n = src.count })\n",
        ),
        &[],
    );
}

#[test]
fn tp9_b2_list_struct_argument_keeps_main_admission() {
    parity(
        &project(
            "fn take(xs: list(struct { name: String })): String { 'x' }\nlet result = take([{ name = src.identifier }])\n",
        ),
        &[],
    );
}

#[test]
fn tp9_b3_unused_struct_return_keeps_main_admission() {
    parity(
        &project("fn mk(x: String): struct { name: String } { { name = x } }\n"),
        &[],
    );
}

#[test]
fn tp9_b7_struct_fields_keep_main_admission() {
    parity(
        &project(
            "fn s2(x: struct { name: String, n: Int }): String { x.name }\nlet mixed = s2({ name = 'x', n = src.count })\nlet named = s2({ name = src.typed_name, n = src.count })\n",
        ),
        &[],
    );
}

#[test]
fn tp9_b7_unknown_struct_and_map_members_need_value_checks() {
    let directory = project(
        "fn sn(x: struct { n: 'dev' | 'prod' }): String { 'x' }\nfn ms(x: map('dev' | 'prod')): String { 'x' }\nlet structure = sn({ n = src.identifier })\nlet mapping = ms({ n = src.identifier })\n",
    );
    parity(
        &directory,
        &[
            "function 'sn': parameter 'x' is declared as struct { n: 'dev' | 'prod' }, but its argument has type map(String), so validate cannot prove it fits. Pass a value known at validate, or give the argument a type that proves it fits.",
            "function 'ms': parameter 'x' is declared as map('dev' | 'prod'), but its argument has type map(String), so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as map(String) and let the resource attribute that receives it check the value.",
        ],
    );
}

#[test]
fn tp9_c1_struct_promise_cannot_hide_main_return_mismatch() {
    parity(
        &project(
            "fn f(a: Int): String { a }\nfn mk(x: String): struct { name: String } { { name = x } }\n",
        ),
        &["function 'f': return value: expected String, got Int"],
    );
}

fn module_outputs(parameter: &str) -> Directory {
    let directory = project(&format!(
        "fn select(x: {parameter}): String {{ x }}\nlet child = use {{ source = './child' }}\nlet output = child {{ n = src.identifier }}\nlet results = for element in output.outs {{ let checked = select(element) test.Thing {{ name = 'loop' }} }}\nlet direct = test.Sink {{ name = 'direct' value = select(output.outs[0]) }}\n"
    ));
    std::fs::create_dir(directory.root().join("child")).unwrap();
    std::fs::write(directory.root().join("child/main.crn"), "arguments { n: String }\nlet r = test.Thing { name = n }\nattributes { outs: list(String) = [r.identifier] }\n").unwrap();
    directory
}

#[test]
fn tp9_c4_module_elements_have_one_complete_finding_in_source_order() {
    let directory = module_outputs(ENVIRONMENT);
    let expected = "function 'select': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.";
    parity(&directory, &[expected, expected]);
    let diagnostics = validate(&directory);
    let lines: Vec<_> = diagnostics.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["location"]["range"]["start"]["line"].as_u64().unwrap())
        .collect();
    assert!(lines[0] < lines[1], "{lines:?}");
}

#[test]
fn tp9_c6_module_output_string_elements_prove_parameter() {
    parity(&module_outputs("String"), &[]);
}

#[test]
fn tp9_c8_typed_upstream_string_elements_prove_parameter() {
    parity(
        &project(
            "let up = upstream_state { source = '../upstream' }\nfn st(x: String): String { x }\nlet results = for element in up.items { let checked = st(element) test.Thing { name = 'loop' } }\nlet direct = test.Sink { name = 'direct' value = st(up.typed) }\n",
        ),
        &[],
    );
}

#[test]
fn tp9_empty_loop_does_not_check_unreached_constant_body_call() {
    parity(
        &project(
            "fn pick(x: 'dev' | 'prod'): String { 'x' }\nfor ignored in [] { test.Thing { name = pick(src.identifier) } }\n",
        ),
        &[],
    );
}

#[test]
fn tp9_promise_does_not_hide_upstream_field_error() {
    let directory = project(
        "let up = upstream_state { source = '../upstream' }\nfn pick(x: 'dev' | 'prod'): String { x }\ntest.Sink { name = 'sink' value = pick(src.identifier) }\nlet missing = up.absent\n",
    );
    let diagnostics = validate(&directory);
    let errors = messages(&diagnostics);
    assert_eq!(errors.len(), 2, "{}", diagnostics.json);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("function 'pick': parameter 'x' is declared as")),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|error| error.contains("absent") && error.contains("export")),
        "{errors:?}"
    );
    let lsp: Vec<_> = lsp_errors(&directory)
        .into_iter()
        .map(|d| d.message)
        .collect();
    let mut actual = errors.to_vec();
    actual.sort_unstable();
    let mut lsp = lsp;
    lsp.sort_unstable();
    assert_eq!(actual, lsp);
}

#[test]
fn tp9_computed_named_struct_member_needs_value_check() {
    parity(
        &project(
            "let a = test.Role { role_name = 'dev' }\nfn take(x: struct { n: test.RoleName }): String { 'accepted' }\nlet result = take({ n = lower(a.computed_name) })\n",
        ),
        &[
            "function 'take': parameter 'x' is declared as struct { n: test.RoleName }, but its argument has type map(String), so validate cannot prove it fits. Pass a value known at validate, or give the argument a type that proves it fits.",
        ],
    );
}
