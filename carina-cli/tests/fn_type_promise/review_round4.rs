use super::*;

fn project(source: &str) -> Directory {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!("{SOURCE}{source}"));
    directory
}

fn parity(directory: &Directory, expected: &[&str]) {
    let cli = validate(directory);
    assert_eq!(messages(&cli), expected, "{}", cli.json);
    let lsp: Vec<_> = lsp_errors(directory)
        .into_iter()
        .map(|finding| finding.message)
        .collect();
    assert_eq!(lsp, expected, "CLI/LSP findings differ");
}

#[test]
fn tp13_struct_known_field_uses_value_check() {
    parity(
        &project(
            "fn s(x: struct { name: String, tag: 'dev' | 'prod' }): String { x.name }\nlet sink = test.Sink { name = 'sink' value = s({ name = src.identifier, tag = 'dev' }) }\n",
        ),
        &[],
    );
}

#[test]
fn tp13_struct_resolved_known_field_uses_value_check() {
    parity(
        &project(
            "let a = test.Role { role_name = 'dev' description = 'dev' }\nfn s(x: struct { name: String, tag: 'dev' | 'prod' }): String { x.name }\nlet sink = test.Sink { name = 'sink' value = s({ name = src.identifier, tag = a.description }) }\n",
        ),
        &[],
    );
}

#[test]
fn tp13_struct_invalid_known_field_keeps_value_error() {
    for tag in ["'qa'", "a.description"] {
        parity(
            &project(&format!(
                "let a = test.Role {{ role_name = 'dev' description = 'qa' }}\nfn s(x: struct {{ name: String, tag: 'dev' | 'prod' }}): String {{ x.name }}\nlet sink = test.Sink {{ name = 'sink' value = s({{ name = src.identifier, tag = {tag} }}) }}\n",
            )),
            &[
                "function 's': parameter 'x': at struct field 'tag': expected 'dev' | 'prod', got string 'qa'",
            ],
        );
    }
}

#[test]
fn tp13_list_known_element_and_proven_unknown_element() {
    parity(
        &project(
            "fn take(x: list(test.Name)): String { 'accepted' }\nlet sink = test.Sink { name = 'sink' value = take(['allowed', src.typed_name]) }\n",
        ),
        &[],
    );
}

#[test]
fn tp13_composite_advice_does_not_suggest_map_for_struct() {
    parity(
        &project(
            "fn s(x: struct { tag: 'dev' | 'prod' }): String { 'accepted' }\nlet sink = test.Sink { name = 'sink' value = s({ tag = src.identifier }) }\n",
        ),
        &[
            "function 's': parameter 'x' is declared as struct { tag: 'dev' | 'prod' }, but its argument has type map(String), so validate cannot prove it fits. Pass a value known at validate, or give the argument a type that proves it fits.",
        ],
    );
}

#[test]
fn tp13_promise_and_mismatch_keep_source_order() {
    let directory = project(
        "fn pick(x: 'dev' | 'prod'): String { 'accepted' }\nlet sink = test.Sink { name = 'sink' value = pick(src.identifier) }\n\nfn bad(a: Int): String { a }\n",
    );
    parity(
        &directory,
        &[
            "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.",
            "function 'bad': return value: expected String, got Int",
        ],
    );
    let cli = validate(&directory);
    let lines: Vec<_> = cli.json["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| {
            error["location"]["range"]["start"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(lines, [3, 5]);
    let lines: Vec<_> = lsp_errors(&directory)
        .into_iter()
        .map(|finding| finding.range.start.line + 1)
        .collect();
    assert_eq!(lines, [3, 5]);
}

#[test]
fn tp13_unknown_return_advice_names_return_value() {
    parity(
        &project(
            "fn r(x: String): 'dev' | 'prod' { lookup({ a = x }, 'a', 'dev') }\nlet sink = test.Sink { name = 'sink' value = r(src.identifier) }\n",
        ),
        &[
            "function 'r': return value is declared as 'dev' | 'prod', but the body has no known type, so validate cannot prove it fits. Pass a value known at validate, or give the return value a type that proves it fits.",
        ],
    );
}
