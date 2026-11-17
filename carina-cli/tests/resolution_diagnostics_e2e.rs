//! Resolution and auxiliary judgements have one publisher, with validate's wording and location.
use carina_core::{
    EnvironmentSnapshot,
    provider::{ProviderFactory, loaded_provider_snapshot_from_factories},
    schema::SchemaRegistry,
};
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use std::{process::Command, sync::Arc};
use tower_lsp::lsp_types::{Diagnostic, Position, Range, TextDocumentContentChangeEvent};

struct Fixture {
    directory: tempfile::TempDir,
    files: Vec<String>,
}
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Finding {
    file: String,
    message: String,
    start: (u32, u32),
    end: (u32, u32),
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        for (file, source) in files {
            let path = directory.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        Self {
            directory,
            files: files.iter().map(|(file, _)| (*file).into()).collect(),
        }
    }
    fn cli(&self, needle: &str) -> Vec<Finding> {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.directory.path())
            .args(["validate", ".", "--json"])
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut findings = report["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["message"].as_str().unwrap().contains(needle))
            .map(|e| {
                let location = &e["location"];
                let point = |edge: &str| {
                    (
                        (location["range"][edge]["line"].as_u64().unwrap() - 1) as u32,
                        (location["range"][edge]["column"].as_u64().unwrap() - 1) as u32,
                    )
                };
                Finding {
                    file: location["file"].as_str().unwrap().into(),
                    message: e["message"].as_str().unwrap().into(),
                    start: point("start"),
                    end: point("end"),
                }
            })
            .collect::<Vec<_>>();
        findings.sort();
        findings
    }
    fn lsp(&self, needle: &str) -> Vec<Finding> {
        let (engine, context) = engine();
        let mut findings = Vec::new();
        for file in &self.files {
            let path = self.directory.path().join(file);
            let document = Document::new_with_path(
                std::fs::read_to_string(&path).unwrap(),
                path.clone(),
                context.clone(),
            );
            findings.extend(
                engine
                    .analyze(&document, Some(&path))
                    .into_iter()
                    .filter(|d| d.message.contains(needle))
                    .map(|diagnostic| finding(file, diagnostic)),
            );
        }
        findings.sort();
        findings
    }
    fn matches_cli(&self, name: &str, needle: &str, count: usize) {
        let expected = self.cli(needle);
        assert_eq!(expected.len(), count, "validate {name}: {expected:#?}");
        assert_eq!(self.lsp(needle), expected, "{name}");
    }
}
fn finding(file: &str, diagnostic: Diagnostic) -> Finding {
    Finding {
        file: file.into(),
        message: diagnostic.message,
        start: (
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        ),
        end: (diagnostic.range.end.line, diagnostic.range.end.character),
    }
}
fn engine() -> (DiagnosticEngine, Arc<carina_core::parser::ProviderContext>) {
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        EnvironmentSnapshot::empty(),
    );
    let context = Arc::new(providers.provider_context().as_provider_context().clone());
    (
        DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]),
        context,
    )
}
fn fixture(name: &str) -> Fixture {
    match name {
        "backend_secret" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = secret('x') key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "backend_sibling_secret" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = state_bucket key = 'state.json' }\n",
            ),
            (
                "providers.crn",
                "provider mock {}\nlet state_bucket = secret('x')\n",
            ),
        ]),
        "backend_sibling_function_secret" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = state_bucket() key = 'state.json' }\n",
            ),
            (
                "providers.crn",
                "provider mock {}\nfn state_bucket() { secret('x') }\n",
            ),
        ]),
        "backend_unresolved_reference" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = missing.name key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "backend_builtin_type_error" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = upper(1) key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "backend_builtin_arity_error" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = upper('a', 'b') key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "backend_config_type_error" => Fixture::new(&[
            ("main.crn", "backend s3 { bucket = 1 key = 'state.json' }\n"),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "sibling_function_body_handle_error" => Fixture::new(&[
            (
                "main.crn",
                "let vpc = mock.test.resource { name = 'vpc' }\nlet bad = identity(vpc)\n",
            ),
            (
                "providers.crn",
                "provider mock {}\nfn identity(r: mock.test.resource): String { r }\n",
            ),
        ]),
        "mixed_backend_secret_provider_finalize" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = secret('x') key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock { default_tags = 123 }\n"),
        ]),
        "backend_env_missing" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = env('CARINA_4492_ABSENT') key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "backend_cidr_invalid_value" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = cidr_subnet('bad', 1, 0) key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "sibling_syntax_secret" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = secret('C4492_PRIVATE') key = 'state.json' }\n",
            ),
            ("providers.crn", "provider mock {}\ninvalid {{{\n"),
        ]),
        "sibling_source_failure_secret" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = secret('C4492_PRIVATE') key = 'state.json' }\n",
            ),
            (
                "providers.crn",
                "provider mock {}\nlet bad = absent_function()\n",
            ),
        ]),
        "sibling_resolution_current_provider_secret" => Fixture::new(&[
            ("main.crn", "mock.test.resource { name = upper(1) }\n"),
            (
                "providers.crn",
                "provider mock {}\nbackend s3 { bucket = secret('C4492_PRIVATE') key = 'state.json' }\n",
            ),
        ]),
        "prefix_empty" => Fixture::new(&[
            ("main.crn", "mock.test.resource { name_prefix = '' }\n"),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "prefix_conflict" => Fixture::new(&[
            (
                "main.crn",
                "mock.test.resource { name = 'x' name_prefix = 'prefix-' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "prefix_loop_empty" => Fixture::new(&[
            (
                "main.crn",
                "for name in ['a', 'b'] { mock.test.resource { name = name name_prefix = '' } }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "module_prefix_empty" => Fixture::new(&[
            (
                "child/main.crn",
                "arguments { value: String }\nmock.test.resource { name_prefix = '' }\n",
            ),
            (
                "main.crn",
                "let child = use { source = './child' }\nchild { value = 'ok' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "module_prefix_conflict" => Fixture::new(&[
            (
                "child/main.crn",
                "arguments { value: String }\nmock.test.resource { name = 'child' name_prefix = 'x' }\n",
            ),
            (
                "main.crn",
                "let child = use { source = './child' }\nchild { value = 'ok' }\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "int_function" => Fixture::new(&[
            ("main.crn", "fn bad(value: String): Int { value + 1 }\n"),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "int_let" => Fixture::new(&[
            ("main.crn", "let source = 'x'\nlet result = source + 1\n"),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "int_div_zero" => Fixture::new(&[
            ("main.crn", "let divisor = 0\nlet result = 1 / divisor\n"),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "int_overflow" => Fixture::new(&[
            (
                "main.crn",
                "let large = 9223372036854775807\nlet result = large + 1\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "int_unknown_ref" => Fixture::new(&[
            (
                "main.crn",
                "let source = mock.test.resource { name = 'x' }\nlet result = source.comment + 1\n",
            ),
            ("providers.crn", "provider mock {}\n"),
        ]),
        "provider_call_bad_return" => Fixture::new(&[
            ("main.crn", "fn bad_tags(): String { true }\n"),
            (
                "providers.crn",
                "provider mock { default_tags = bad_tags() }\n",
            ),
        ]),
        "shared_function_backend_provider" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = bad_tags() key = 'state.json' }\n",
            ),
            (
                "providers.crn",
                "provider mock { default_tags = bad_tags() }\nfn bad_tags(): String { true }\n",
            ),
        ]),
        "shared_function_let_provider" => Fixture::new(&[
            ("main.crn", "let root_result = bad_tags()\n"),
            (
                "providers.crn",
                "provider mock { default_tags = bad_tags() }\nfn bad_tags(): String { true }\n",
            ),
        ]),
        "shared_function_resource_provider" => Fixture::new(&[
            ("main.crn", "mock.test.resource { name = bad_tags() }\n"),
            (
                "providers.crn",
                "provider mock { default_tags = bad_tags() }\nfn bad_tags(): String { true }\n",
            ),
        ]),
        "shared_function_body_backend_provider" => Fixture::new(&[
            (
                "main.crn",
                "backend s3 { bucket = bad_tags() key = 'state.json' }\n",
            ),
            (
                "providers.crn",
                "provider mock { default_tags = bad_tags() }\nfn bad_tags(): String { upper(1) }\n",
            ),
        ]),
        "shared_handle_root_provider" => Fixture::new(&[
            (
                "main.crn",
                "let vpc = mock.test.resource { name = 'vpc' }\nlet root_result = identity(vpc)\n",
            ),
            (
                "providers.crn",
                "provider mock { default_tags = identity(vpc) }\nfn identity(r: mock.test.resource): String { r }\n",
            ),
        ]),
        _ => panic!("unknown fixture: {name}"),
    }
}

macro_rules! cli_case {
    ($(#[$attribute:meta])* $test:ident, $name:literal, $needle:literal, $count:literal) => {
        $(#[$attribute])*
        #[test]
        fn $test() {
            fixture($name).matches_cli($name, $needle, $count);
            if $name.starts_with("shared_") {
                assert!(
                    fixture($name).lsp("default_tags").is_empty(),
                    "{}: rejected body produced a provider-map consequence",
                    $name
                );
            }
        }
    };
}

cli_case!(
    backend_secret_has_one_owner,
    "backend_secret",
    "backend s3 attribute",
    1
);
cli_case!(
    backend_builtin_type_error_has_one_owner,
    "backend_builtin_type_error",
    "upper()",
    1
);
cli_case!(
    backend_builtin_arity_error_has_one_owner,
    "backend_builtin_arity_error",
    "upper()",
    1
);
cli_case!(
    backend_env_missing_has_one_owner,
    "backend_env_missing",
    "env()",
    1
);
cli_case!(
    backend_cidr_invalid_value_has_one_owner,
    "backend_cidr_invalid_value",
    "cidr_subnet()",
    1
);

#[test]
fn sibling_syntax_recovery_has_one_resolution_owner() {
    assert_eq!(
        fixture("sibling_syntax_secret").lsp("backend s3 attribute"),
        fixture("backend_secret").cli("backend s3 attribute")
    );
}
#[test]
fn sibling_source_recovery_has_one_resolution_owner() {
    assert_eq!(
        fixture("sibling_source_failure_secret").lsp("backend s3 attribute"),
        fixture("backend_secret").cli("backend s3 attribute")
    );
}

cli_case!(
    shared_function_backend_provider_has_one_owner,
    "shared_function_backend_provider",
    "function 'bad_tags'",
    1
);
cli_case!(
    shared_function_let_provider_has_one_owner,
    "shared_function_let_provider",
    "function 'bad_tags'",
    1
);
cli_case!(
    shared_function_resource_provider_has_one_owner,
    "shared_function_resource_provider",
    "function 'bad_tags'",
    1
);
cli_case!(
    shared_function_body_backend_provider_has_one_owner,
    "shared_function_body_backend_provider",
    "upper()",
    1
);
cli_case!(
    shared_handle_root_provider_has_one_owner,
    "shared_handle_root_provider",
    "resource handle",
    1
);

cli_case!(
    int_function_has_one_owner,
    "int_function",
    "operator '+'",
    1
);
cli_case!(
    int_unknown_ref_has_one_owner,
    "int_unknown_ref",
    "operator '+'",
    1
);
cli_case!(int_let_has_one_owner, "int_let", "operator '+'", 1);
cli_case!(
    int_div_zero_has_one_owner,
    "int_div_zero",
    "division by zero",
    1
);
cli_case!(int_overflow_has_one_owner, "int_overflow", "overflow", 1);

cli_case!(
    #[ignore = "carina#4492: prefix owner pending"]
    prefix_empty_has_one_owner,
    "prefix_empty",
    "'name_prefix'",
    1
);
cli_case!(
    #[ignore = "carina#4492: prefix owner pending"]
    prefix_conflict_has_one_owner,
    "prefix_conflict",
    "'name_prefix'",
    1
);
cli_case!(
    #[ignore = "carina#4492: prefix owner pending"]
    module_prefix_empty_has_one_owner,
    "module_prefix_empty",
    "'name_prefix'",
    1
);
cli_case!(
    #[ignore = "carina#4492: prefix owner pending"]
    module_prefix_conflict_has_one_owner,
    "module_prefix_conflict",
    "'name_prefix'",
    1
);
cli_case!(
    #[ignore = "carina#4492: prefix owner pending"]
    prefix_loop_empty_has_one_owner,
    "prefix_loop_empty",
    "'name_prefix'",
    2
);

cli_case!(
    backend_sibling_secret_has_one_owner,
    "backend_sibling_secret",
    "backend s3 attribute",
    1
);
cli_case!(
    backend_sibling_function_secret_has_one_owner,
    "backend_sibling_function_secret",
    "backend s3 attribute",
    1
);
cli_case!(
    sibling_function_body_handle_error_has_one_owner,
    "sibling_function_body_handle_error",
    "'r' is a resource handle",
    1
);

#[test]
fn backend_secret_has_exact_key_range_and_no_provider_finding() {
    let findings = fixture("backend_secret").lsp("backend s3 attribute");
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert_eq!(findings[0].file, "main.crn");
    assert_eq!(findings[0].start, (0, 13));
    assert_eq!(findings[0].end, (0, 19));
    assert_eq!(
        findings[0].message,
        "Invalid expression at line 1: backend s3 attribute 'bucket' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed"
    );
    assert_eq!(fixture("backend_secret").lsp("").len(), 1);
    assert_eq!(
        fixture("backend_builtin_type_error").cli("upper()")[0].message,
        "upper(): upper() argument must be a string, got Int"
    );
}

fn assert_unsaved_resolution(orphan: bool, broken_sibling: bool) {
    const SECRET: &str = "backend s3 { bucket = secret('C4492_PRIVATE') key = 'state.json' }\n";
    const PLAIN: &str = "backend s3 { bucket = 'state' key = 'state.json' }\n";
    let saved = Fixture::new(&[
        ("main.crn", SECRET),
        ("providers.crn", "provider mock {}\n"),
    ]);
    let expected = saved.cli("backend s3 attribute");
    assert_eq!(expected.len(), 1);
    let edited = Fixture::new(&[("main.crn", PLAIN), ("providers.crn", "provider mock {}\n")]);
    if broken_sibling {
        std::fs::write(
            edited.directory.path().join("broken.crn"),
            "let broken = [\n",
        )
        .unwrap();
    }
    let path = edited.directory.path().join("main.crn");
    let current_path = (!orphan).then_some(path.as_path());
    let (engine, context) = engine();
    let mut document = Document::new_with_path(PLAIN.into(), path.clone(), context);
    document.apply_change(TextDocumentContentChangeEvent {
        range: None,
        range_length: None,
        text: SECRET.into(),
    });
    let introduced = engine
        .analyze(&document, current_path)
        .into_iter()
        .filter(|d| d.message.contains("backend s3 attribute"))
        .map(|d| finding("main.crn", d))
        .collect::<Vec<_>>();
    // The disk now retains the secret while the unsaved buffer removes it.
    std::fs::write(&path, SECRET).unwrap();
    document.apply_change(TextDocumentContentChangeEvent {
        range: None,
        range_length: None,
        text: PLAIN.into(),
    });
    let removed = engine.analyze(&document, current_path);
    assert!(
        removed
            .iter()
            .all(|d| !d.message.contains("backend s3 attribute")),
        "{removed:#?}"
    );
    assert_eq!(
        introduced, expected,
        "orphan={orphan}, broken={broken_sibling}"
    );
}
#[test]
fn orphan_resolution_finding_has_one_owner() {
    assert_unsaved_resolution(true, false);
}
#[test]
fn unsaved_introduce_remove_uses_one_resolution_owner() {
    assert_unsaved_resolution(false, false);
}
#[test]
fn recovered_unsaved_resolution_uses_one_owner() {
    assert_unsaved_resolution(false, true);
}

#[test]
fn sibling_resolution_failure_preserves_current_file_fallback() {
    let fixture = fixture("sibling_resolution_current_provider_secret");
    let path = fixture.directory.path().join("providers.crn");
    let (engine, context) = engine();
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        context,
    );
    let matching = engine
        .analyze(&document, Some(&path))
        .into_iter()
        .filter(|d| d.message.contains("backend s3 attribute"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "{matching:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(1, 13), Position::new(1, 19))
    );
    assert!(
        matching[0]
            .message
            .starts_with("Invalid expression at line 2: backend s3 attribute")
    );
}

#[test]
fn literal_provider_argument_and_broken_body_have_one_owner_each() {
    let fixture = Fixture::new(&[
        ("main.crn", "let root_result = broken('root')\n"),
        (
            "providers.crn",
            "provider mock { default_tags = broken(1) }\nfn broken(value: String): String { true }\n",
        ),
    ]);
    // Analyze both files: the provider argument is judged by root resolution,
    // while the retained report owns the separate return-body error.
    assert_eq!(
        fixture.lsp(""),
        vec![
            Finding {
                file: "providers.crn".into(),
                message: "function 'broken': parameter 'value': expected String, got int (1)"
                    .into(),
                start: (0, 31),
                end: (0, 40),
            },
            Finding {
                file: "providers.crn".into(),
                message: "function 'broken': return value: expected String, got Bool".into(),
                start: (1, 35),
                end: (1, 39),
            },
        ],
    );
}

#[test]
fn equal_arguments_at_distinct_calls_keep_independent_provider_findings() {
    let fixture = Fixture::new(&[
        ("main.crn", "let root_result = broken(1)\n"),
        (
            "providers.crn",
            "provider mock { default_tags = broken(1) }\nfn broken(value: String): String { true }\n",
        ),
    ]);
    assert_eq!(
        fixture.lsp("function 'broken': parameter 'value': expected String, got int (1)"),
        vec![
            Finding {
                file: "main.crn".into(),
                message: "function 'broken': parameter 'value': expected String, got int (1)"
                    .into(),
                start: (0, 18),
                end: (0, 27),
            },
            Finding {
                file: "providers.crn".into(),
                message: "function 'broken': parameter 'value': expected String, got int (1)"
                    .into(),
                start: (0, 31),
                end: (0, 40),
            },
        ],
    );
}

#[test]
fn provider_only_and_independent_provider_errors_are_preserved() {
    let provider_only = fixture("provider_call_bad_return");
    let finding = provider_only.lsp("function 'bad_tags'");
    assert_eq!(finding.len(), 1, "{finding:#?}");
    assert_eq!(finding[0].file, "main.crn");
    for same_file in [false, true] {
        let source = "backend s3 { bucket = secret('x') key = 'state.json' }\n";
        let provider = "provider mock { default_tags = 123 }\n";
        let fixture = if same_file {
            Fixture::new(&[("main.crn", &(source.to_owned() + provider))])
        } else {
            Fixture::new(&[("main.crn", source), ("providers.crn", provider)])
        };
        assert_eq!(fixture.lsp("backend s3 attribute").len(), 1);
        let findings = fixture.lsp("default_tags");
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert!(findings[0].message.contains("map"), "{findings:#?}");
    }
}

#[test]
fn independent_backend_schema_and_reference_errors_are_preserved() {
    fixture("backend_config_type_error").matches_cli(
        "backend_config_type_error",
        "Invalid attribute",
        1,
    );
    let unresolved = fixture("backend_unresolved_reference");
    unresolved.matches_cli("backend_unresolved_reference", "Invalid attribute", 1);
    assert_eq!(unresolved.lsp("Undefined resource: 'missing'").len(), 1);
}

#[test]
fn body_failure_keeps_independent_provider_and_reference_findings() {
    let fixture = Fixture::new(&[
        (
            "main.crn",
            "let root_result = bad_tags()\nmock.test.resource { name = missing.name }\n",
        ),
        (
            "providers.crn",
            "provider mock { default_tags = 123 }\nfn bad_tags(): String { true }\n",
        ),
    ]);
    assert_eq!(fixture.lsp("function 'bad_tags'").len(), 1);
    assert_eq!(fixture.lsp("default_tags").len(), 1);
    assert_eq!(fixture.lsp("Undefined resource: 'missing'").len(), 1);
    let independent_functions = Fixture::new(&[
        (
            "main.crn",
            "let root_result = bad_root()\nfn bad_root(): String { true }\n",
        ),
        (
            "providers.crn",
            "provider mock { default_tags = bad_provider() }\nfn bad_provider(): String { false }\n",
        ),
    ]);
    assert_eq!(independent_functions.lsp("function 'bad_root'").len(), 1);
    assert_eq!(
        independent_functions.lsp("function 'bad_provider'").len(),
        1
    );
    let argument_sensitive = Fixture::new(&[
        (
            "main.crn",
            "let source = mock.test.resource { name = 'x' comment = 'ok' }\nlet root_result = label(1)\n",
        ),
        (
            "providers.crn",
            "provider mock { default_tags = label(source.comment) }\nfn label(value) { upper(value) }\n",
        ),
    ]);
    assert_eq!(argument_sensitive.lsp("upper() argument").len(), 1);
    assert_eq!(argument_sensitive.lsp("default_tags").len(), 1);
}

#[test]
fn duplicate_declarations_precede_the_single_resolution_finding() {
    let fixture = Fixture::new(&[(
        "main.crn",
        "provider mock {}\nprovider mock {}\nbackend s3 { bucket = secret('x') key = 'state.json' }\n",
    )]);
    let path = fixture.directory.path().join("main.crn");
    let (engine, context) = engine();
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        context,
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let secrets = diagnostics
        .iter()
        .filter(|d| d.message.contains("backend s3 attribute"))
        .collect::<Vec<_>>();
    assert_eq!(secrets.len(), 1, "{diagnostics:#?}");
    let duplicate = diagnostics
        .iter()
        .position(|d| d.message.to_ascii_lowercase().contains("duplicate"))
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    let resolution = diagnostics
        .iter()
        .position(|d| d.message.contains("backend s3 attribute"))
        .unwrap();
    assert!(duplicate < resolution, "{diagnostics:#?}");
}

#[test]
fn independent_provider_argument_failure_survives_owned_function_body() {
    let fixture = Fixture::new(&[
        (
            "main.crn",
            "let wrong = mock.test.resource { name = 'x' comment = 1 }\nlet root_result = broken('root')\n",
        ),
        (
            "providers.crn",
            "provider mock { default_tags = broken(wrong.comment) }\nfn broken(value: String): String { true }\n",
        ),
    ]);
    let findings = fixture.lsp("function 'broken'");
    assert_eq!(findings.len(), 2, "{findings:#?}");
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.message.contains("return value"))
            .count(),
        1,
        "{findings:#?}"
    );
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.message.contains("parameter 'value'"))
            .count(),
        1,
        "{findings:#?}"
    );
}
