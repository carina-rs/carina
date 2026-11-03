//! Review regressions: validate must preserve main's findings (#4220).
use std::path::Path;
use std::process::{Command, Output};

fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    dir
}

fn validate(dir: &Path) -> (Output, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "."])
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output, text)
}

fn assert_cycle(files: &[(&str, &str)], expected: &str, anchor: &str) {
    let dir = fixture(files);
    let (output, text) = validate(dir.path());
    assert!(!output.status.success(), "{text}");
    let cycles: Vec<_> = text
        .lines()
        .filter(|line| line.contains("Circular dependency detected:"))
        .collect();
    assert_eq!(cycles.len(), 1, "{text}");
    assert!(cycles[0].ends_with(expected), "{text}");
    assert!(cycles[0].contains(anchor), "{text}");
    assert!(!cycles[0].contains("warning:"), "{text}");
}

#[test]
fn cycle_is_one_cli_error() {
    assert_cycle(
        &[
            (
                "main.crn",
                "provider mock {}\nlet a = mock.test.resource { name = 'a' directives { depends_on = [b] } }\n",
            ),
            (
                "sibling.crn",
                "let b = mock.test.resource { name = a.name }\n",
            ),
        ],
        "Circular dependency detected: a -> b -> a",
        "main.crn:2:68:",
    );
}

#[test]
fn cycle_preserves_main_ring_order_and_first_cycle() {
    assert_cycle(
        &[
            (
                "main.crn",
                "provider mock {}\nlet a = mock.test.resource { name = 'a' directives { depends_on = [c] } }\nlet b = mock.test.resource { name = a.name }\n",
            ),
            (
                "sibling.crn",
                "let c = mock.test.resource { name = b.name }\nlet d = mock.test.resource { name = 'd' directives { depends_on = [e] } }\nlet e = mock.test.resource { name = d.name }\n",
            ),
        ],
        "Circular dependency detected: a -> c -> b -> a",
        "main.crn:2:68:",
    );
}

#[test]
fn cycle_includes_bare_binding_dependencies() {
    assert_cycle(
        &[
            (
                "main.crn",
                "provider mock {}\nlet a = mock.test.resource { name = 'a' directives { depends_on = [b] } }\n",
            ),
            (
                "sibling.crn",
                "fn pick(resource: mock.test.resource, value: String): String { value }\nlet c = mock.test.resource { name = 'c' }\nlet b = mock.test.resource { name = pick(a, c.identifier) }\n",
            ),
        ],
        "Circular dependency detected: a -> b -> a",
        "main.crn:2:68:",
    );
}

#[test]
fn cycle_includes_aliased_binding_dependencies() {
    assert_cycle(
        &[
            (
                "main.crn",
                "provider mock {}\nlet a = mock.test.resource { name = 'a' directives { depends_on = [b] } }\n",
            ),
            (
                "sibling.crn",
                "fn pick(resource: mock.test.resource, value: String): String { value }\nlet alias = a\nlet c = mock.test.resource { name = 'c' }\nlet b = mock.test.resource { name = pick(alias, c.identifier) }\n",
            ),
        ],
        "Circular dependency detected: a -> b -> a",
        "main.crn:2:68:",
    );
}

#[test]
fn cycle_includes_selected_binding_dependencies() {
    assert_cycle(
        &[
            (
                "main.crn",
                "provider mock {}\nlet copies = for n in [1] {\n  mock.test.resource { name = b.name directives { depends_on = [b] } }\n}\n",
            ),
            (
                "sibling.crn",
                "fn pick(resource: mock.test.resource, value: String): String { value }\nlet c = mock.test.resource { name = 'c' }\nlet b = mock.test.resource { name = pick(copies[0], c.identifier) }\n",
            ),
        ],
        "Circular dependency detected: copies[0] -> copies[0]",
        "main.crn:3:",
    );
}

#[test]
fn module_handle_dependency_origins_preserve_validation() {
    let dir = fixture(&[
        (
            "main.crn",
            "provider mock {}\nlet source = mock.test.resource { name = 'source' }\nlet other = mock.test.resource { name = 'other' }\nlet child = use { source = './child' }\nlet instance = child { selected = source value = other.identifier }\n",
        ),
        (
            "child/main.crn",
            "arguments { selected: mock.test.resource value: String }\nfn pick(resource: mock.test.resource, value: String): String { value }\n",
        ),
        (
            "child/sibling.crn",
            "mock.test.resource { name = pick(selected, value) }\n",
        ),
    ]);
    let (output, text) = validate(dir.path());
    assert!(output.status.success(), "{text}");
}

fn upstream_fixture(source: &str) -> tempfile::TempDir {
    fixture(&[
        (
            "up/exports.crn",
            "exports { names: list(String) = ['one'] scalar: String = 'one' }\n",
        ),
        (
            "down/main.crn",
            "provider mock {}\nlet up = upstream_state { source = '../up' }\n",
        ),
        ("down/sibling.crn", source),
    ])
}

#[test]
fn upstream_shapes_skip_unused_let_values() {
    let dir = upstream_fixture("let field = up.names.bad\nlet index = up.names['bad']\n");
    let (output, text) = validate(&dir.path().join("down"));
    assert!(output.status.success(), "{text}");
    assert!(
        !text.contains("reads it as a struct") && !text.contains("does not fit"),
        "{text}"
    );
}

#[test]
fn upstream_shapes_skip_attribute_parameter_values() {
    let dir = upstream_fixture(
        "attributes { field: String = up.names.bad index: String = up.names['bad'] }\n",
    );
    let (output, text) = validate(&dir.path().join("down"));
    assert!(output.status.success(), "{text}");
    assert!(
        !text.contains("reads it as a struct") && !text.contains("does not fit"),
        "{text}"
    );
}

#[test]
fn identity_conflict_stops_before_upstream_shapes() {
    let dir = upstream_fixture(
        "mock.test.Thing { name = 'alpha' }\nmock.test.Thing { name = 'beta' }\nlet consumer = mock.test.resource { name = 'valid' }\n",
    );
    let down = dir.path().join("down");
    let (identity_output, identity_text) = validate(&down);
    assert!(!identity_output.status.success(), "{identity_text}");
    assert!(
        identity_text.contains("Anonymous resource identity is derived from mutable attributes"),
        "{identity_text}"
    );
    assert_eq!(
        identity_text.matches("Error: ").count(),
        1,
        "{identity_text}"
    );

    std::fs::write(
        down.join("sibling.crn"),
        "mock.test.Thing { name = 'alpha' }\nmock.test.Thing { name = 'beta' }\nlet consumer = mock.test.resource { name = up.names.bad }\n",
    )
    .unwrap();
    let (output, text) = validate(&down);
    assert_eq!(
        output.status.code(),
        identity_output.status.code(),
        "{text}"
    );
    assert_eq!(
        text, identity_text,
        "main stops at the identity error before checking the sibling's upstream reference"
    );
}

#[test]
fn upstream_expanded_copies_keep_each_context() {
    for expression in ["up.names.bad", "up.names['bad']"] {
        let dir = upstream_fixture(&format!(
            "let copies = for n in [1, 2] {{\n  mock.test.resource {{ name = {expression} }}\n}}\n"
        ));
        let (output, text) = validate(&dir.path().join("down"));
        assert!(!output.status.success(), "{text}");
        let findings: Vec<_> = text
            .lines()
            .filter(|line| line.contains("upstream_state `up.names`") && line.contains("bad"))
            .collect();
        assert_eq!(findings.len(), 2, "{text}");
        assert!(
            findings
                .iter()
                .all(|line| line.contains("sibling.crn:2:31:")),
            "{text}"
        );
        assert_ne!(
            findings[0], findings[1],
            "each generated resource keeps its own diagnostic context: {text}"
        );
    }
}

#[test]
fn upstream_iterable_aliases_keep_each_occurrence() {
    let dir = upstream_fixture(
        "let values = up.scalar\nlet copies = for n in (values ?? values) { mock.test.resource { name = n } }\n",
    );
    let (output, text) = validate(&dir.path().join("down"));
    assert!(!output.status.success(), "{text}");
    let findings: Vec<_> = text
        .lines()
        .filter(|line| {
            line.contains("upstream_state `up.scalar`") && line.contains("requires a list iterable")
        })
        .collect();
    assert_eq!(findings.len(), 2, "{text}");
    assert!(
        findings
            .iter()
            .all(|line| line.contains("sibling.crn:1:14:")),
        "{text}"
    );
}

#[test]
fn upstream_iterable_keeps_repeated_occurrences() {
    let dir = upstream_fixture(
        "let copies = for n in (up.names.bad ?? up.names.bad) {\n  mock.test.resource { name = n }\n}\n",
    );
    let (output, text) = validate(&dir.path().join("down"));
    assert!(!output.status.success(), "{text}");
    let findings: Vec<_> = text
        .lines()
        .filter(|line| line.contains("upstream_state `up.names`") && line.contains("bad"))
        .collect();
    assert_eq!(findings.len(), 2, "{text}");
    assert!(
        findings
            .iter()
            .any(|line| line.contains("sibling.crn:1:24:")),
        "{text}"
    );
    assert!(
        findings
            .iter()
            .any(|line| line.contains("sibling.crn:1:40:")),
        "{text}"
    );
}
