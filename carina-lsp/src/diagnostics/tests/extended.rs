use super::*;
use crate::backend::document_end_position;
use carina_core::parser::TypeExpr;
use carina_core::resource::Value;
use tower_lsp::lsp_types::{Position, Range, TextDocumentContentChangeEvent};

const ATTRIBUTE_DERIVED_IDENTITY_ERROR: &str = "Anonymous resource identity is derived from \
mutable attributes for multiple 'mock.test.resource' declarations using provider instance \
'<default>' in the root scope. Use `let` bindings to give them distinct stable identities.";

fn anonymous_loop_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("mutable-identity fixture schema must be valid"),
    );
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource").attribute(
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("stable-identity fixture schema must be valid"),
    );
    custom_engine(schemas)
}

fn analyze_anonymous_loop_source(source: &str) -> Vec<Diagnostic> {
    let temp = tempfile::tempdir().expect("create diagnostic fixture");
    std::fs::write(temp.path().join("main.crn"), source).expect("write main fixture");
    std::fs::write(temp.path().join("providers.crn"), "provider mock {}\n")
        .expect("write provider fixture");
    let path = temp.path().join("main.crn");
    anonymous_loop_engine().analyze(&create_document_at(source, path.clone()), Some(&path))
}

fn only_message_diagnostic<'a>(diagnostics: &'a [Diagnostic], needle: &str) -> &'a Diagnostic {
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains(needle))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    matching[0]
}

#[test]
fn anonymous_loop_attribute_derived_diagnostic_has_exact_range() {
    let source = "for name in ['alpha', 'beta'] {\n  mock.test.Thing {\n    name = name\n  }\n}\n";
    let diagnostics = analyze_anonymous_loop_source(source);
    let diagnostic =
        only_message_diagnostic(&diagnostics, "derive identity from mutable attributes");

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(0, 0), Position::new(4, 1))
    );
    assert_eq!(
        diagnostic.message,
        concat!(
            "Anonymous resources produced by the unbound `for` at main.crn:1 cannot be ",
            "identified safely because multiple 'mock.test.Thing' elements derive identity ",
            "from mutable attributes.\n",
            "Rewrite the loop with a binding and migrate its existing state addresses:\n\n",
            "let thing = for name in ['alpha', 'beta'] {\n",
            "  mock.test.Thing {\n",
            "    name = name\n",
            "  }\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.Thing '_for0[0]'\n",
            "  to = mock.test.Thing 'thing[0]'\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.Thing '_for0[1]'\n",
            "  to = mock.test.Thing 'thing[1]'\n",
            "}",
        )
    );
}

#[test]
fn anonymous_plain_and_single_loop_member_both_receive_diagnostics() {
    let source = concat!(
        "mock.test.Thing { name = 'plain' }\n",
        "for name in ['loop'] {\n",
        "  mock.test.Thing { name = name }\n",
        "}\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);

    let plain = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .starts_with("Anonymous resource identity is derived from mutable attributes")
                && diagnostic.message.contains("'mock.test.Thing'")
        })
        .collect::<Vec<_>>();
    let loops = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("produced by the unbound `for`"))
        .collect::<Vec<_>>();
    assert_eq!(plain.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(loops.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        loops[0].range,
        Range::new(Position::new(1, 0), Position::new(3, 1))
    );
}

#[test]
fn distinct_single_element_loops_receive_two_diagnostics() {
    let source = concat!(
        "for name in ['alpha'] {\n  mock.test.Thing { name = name }\n}\n",
        "for name in ['beta'] {\n  mock.test.Thing { name = name }\n}\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);
    let loops = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("produced by the unbound `for`"))
        .collect::<Vec<_>>();

    assert_eq!(loops.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(loops[0].range.start.line, 0);
    assert_eq!(loops[1].range.start.line, 3);
}

#[test]
fn sibling_single_element_loops_each_receive_their_own_diagnostic() {
    let temp = tempfile::tempdir().expect("create diagnostic fixture");
    let a = "for name in ['alpha'] {\n  mock.test.Thing { name = name }\n}\n";
    let b = "for name in ['beta'] {\n  mock.test.Thing { name = name }\n}\n";
    std::fs::write(temp.path().join("a.crn"), a).expect("write a fixture");
    std::fs::write(temp.path().join("b.crn"), b).expect("write b fixture");
    std::fs::write(temp.path().join("providers.crn"), "provider mock {}\n")
        .expect("write provider fixture");

    for (file, source) in [("a.crn", a), ("b.crn", b)] {
        let path = temp.path().join(file);
        let diagnostics =
            anonymous_loop_engine().analyze(&create_document_at(source, path.clone()), Some(&path));
        let loops = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("produced by the unbound `for`"))
            .collect::<Vec<_>>();
        assert_eq!(loops.len(), 1, "{file} diagnostics: {diagnostics:#?}");
        assert!(loops[0].message.contains(&format!("at {file}:1")));
    }
}

#[test]
fn anonymous_loop_identical_identity_diagnostic_has_exact_range() {
    let source =
        "for name in ['same', 'same'] {\n  mock.test.resource {\n    name = name\n  }\n}\n";
    let diagnostics = analyze_anonymous_loop_source(source);
    let diagnostic = only_message_diagnostic(&diagnostics, "identical stable identity inputs");

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(0, 0), Position::new(4, 1))
    );
    assert!(
        diagnostic
            .message
            .contains("let resource = for name in ['same', 'same']")
    );
    assert!(
        diagnostic
            .message
            .contains("from = mock.test.resource '_for0[1]'")
    );
    assert!(
        diagnostic
            .message
            .contains("to = mock.test.resource 'resource[1]'")
    );
}

#[test]
fn anonymous_loop_moved_to_legacy_diagnostic_has_exact_range() {
    let source = concat!(
        "moved {\n",
        "  from = mock.test.resource 'old'\n",
        "  to = mock.test.resource '_for0[0]'\n",
        "}\n\n",
        "for name in ['one'] {\n",
        "  mock.test.resource {\n",
        "    name = name\n",
        "  }\n",
        "}\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);
    let diagnostic = only_message_diagnostic(&diagnostics, "legacy synthetic loop address");

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(2, 7), Position::new(2, 36))
    );
    assert_eq!(
        diagnostic.message,
        concat!(
            "The `moved.to` address mock.test.resource '_for0[0]' is a legacy synthetic loop ",
            "address and cannot be a destination. Use the current anonymous address ",
            "mock.test.resource 'mock_test_resource_96081a11' instead.",
        )
    );
}

#[test]
fn anonymous_loop_import_to_legacy_diagnostic_has_exact_range() {
    let source = concat!(
        "import {\n",
        "  to = mock.test.resource '_for0[0]'\n",
        "  id = 'resource-existing'\n",
        "}\n\n",
        "for name in ['one'] {\n",
        "  mock.test.resource {\n",
        "    name = name\n",
        "  }\n",
        "}\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);
    let diagnostic = only_message_diagnostic(&diagnostics, "legacy synthetic loop address");

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(1, 7), Position::new(1, 36))
    );
    assert_eq!(
        diagnostic.message,
        concat!(
            "The `import.to` address mock.test.resource '_for0[0]' is a legacy synthetic loop ",
            "address and cannot be a destination. Use the current anonymous address ",
            "mock.test.resource 'mock_test_resource_96081a11' instead.",
        )
    );
}

#[test]
fn plain_anonymous_identity_collision_is_reported_on_each_authored_resource() {
    let source = concat!(
        "mock.test.resource { name = 'same' }\n",
        "\n",
        "mock.test.resource { name = 'same' }\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);
    let collisions = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Anonymous resource identifier collision")
        })
        .collect::<Vec<_>>();

    assert_eq!(collisions.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(collisions[0].range.start.line, 0);
    assert_eq!(collisions[1].range.start.line, 2);
}

#[test]
fn anonymous_identity_and_legacy_move_target_diagnostics_are_both_reported() {
    let source = concat!(
        "moved {\n",
        "  from = mock.test.resource 'old'\n",
        "  to = mock.test.resource '_for9[0]'\n",
        "}\n\n",
        "for name in ['same', 'same'] {\n",
        "  mock.test.resource { name = name }\n",
        "}\n",
    );
    let diagnostics = analyze_anonymous_loop_source(source);

    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic
                .message
                .contains("identical stable identity inputs"))
            .count(),
        1,
        "identity diagnostic missing: {diagnostics:#?}",
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("legacy synthetic loop address"))
            .count(),
        1,
        "legacy moved.to diagnostic missing: {diagnostics:#?}",
    );
}

#[test]
fn unbound_loop_read_diagnostics_require_let_at_the_loop_range() {
    for (source, kind) in [
        (
            "for name in ['one'] {\n  read mock.test.resource { name = name }\n}\n",
            "for",
        ),
        (
            "if true {\n  read mock.test.resource { name = 'one' }\n}\n",
            "if",
        ),
    ] {
        let diagnostics = analyze_anonymous_loop_source(source);
        let diagnostic = only_message_diagnostic(&diagnostics, "cannot contain `read`");
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(2, 1)),
            "{kind} diagnostic range",
        );
        assert!(
            diagnostic
                .message
                .contains(&format!("bind the {kind} expression with `let`")),
            "diagnostic: {diagnostic:#?}",
        );
    }
}

fn backend_directory_diagnostics(source: &str) -> Vec<Diagnostic> {
    backend_directory_diagnostics_with_sibling("\n", source)
}

fn backend_directory_diagnostics_with_sibling(
    sibling_source: &str,
    backend_source: &str,
) -> Vec<Diagnostic> {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), sibling_source).unwrap();
    std::fs::write(fixture.path().join("backend.crn"), backend_source).unwrap();
    let path = fixture.path().join("backend.crn");
    test_engine().analyze(
        &create_document_at(backend_source, path.clone()),
        Some(&path),
    )
}

fn assert_backend_typo_is_diagnosed(sibling_source: &str, backend_source: &str) {
    let closing_brace = backend_source
        .rfind('}')
        .expect("backend fixture must have a closing brace");
    let backend_with_typo = format!(
        "{}  typo = true\n{}",
        &backend_source[..closing_brace],
        &backend_source[closing_brace..]
    );
    let diagnostics =
        backend_directory_diagnostics_with_sibling(sibling_source, &backend_with_typo);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Unknown attribute 'typo'")),
        "positive-control diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn backend_schema_diagnostics_are_directory_scoped_and_attribute_anchored() {
    for (source, needle, line) in [
        (
            "backend s3 {\n  bucket = \"state\"\n  key = \"state.json\"\n  typo = true\n}\n",
            "Unknown attribute 'typo'",
            3,
        ),
        (
            "backend s3 {\n  bucket = \"state\"\n  key = \"state.json\"\n  region = true\n}\n",
            "Invalid attribute 'region'",
            3,
        ),
    ] {
        let diagnostics = backend_directory_diagnostics(source);
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}: {diagnostics:#?}"));
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.range.start.line, line);
        assert_eq!(diagnostic.range.start.character, 2);
    }
}

#[test]
fn backend_schema_is_not_reported_for_a_module_definition() {
    let backend = "backend s3 {\n  bucket = \"state\"\n  key = \"state.json\"\n  typo = true\n}\n";
    let module_diagnostics =
        backend_directory_diagnostics_with_sibling("arguments {\n  name: String\n}\n", backend);
    let module_messages: Vec<_> = module_diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .filter(|message| {
            message.contains("backend blocks are not allowed inside modules")
                || message.contains("Unknown attribute 'typo'")
        })
        .collect();
    assert_eq!(
        module_messages,
        vec![
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
        ],
        "module diagnostics: {module_diagnostics:#?}"
    );

    let root_diagnostics = backend_directory_diagnostics(backend);
    let root_messages: Vec<_> = root_diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .filter(|message| {
            message.contains("backend blocks are not allowed inside modules")
                || message.contains("Unknown attribute 'typo'")
        })
        .collect();
    assert_eq!(
        root_messages,
        vec!["Unknown attribute 'typo' for backend type 's3'"],
        "root diagnostics: {root_diagnostics:#?}"
    );
}

#[test]
fn backend_enum_region_has_no_schema_diagnostic_in_directory() {
    let source = "backend s3 {\n  bucket = \"state\"\n  key = \"state.json\"\n  region = aws.Region.us_west_2\n}\n";
    let diagnostics = backend_directory_diagnostics(source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("backend type 's3'")
                && !diagnostic.message.contains("attribute 'region'")
        }),
        "diagnostics: {diagnostics:#?}"
    );
    assert_backend_typo_is_diagnosed("\n", source);
}

#[test]
fn backend_schema_uses_resolved_sibling_let_and_function_values() {
    for (sibling_source, bucket_expression) in [
        ("let state_bucket_name = \"state\"\n", "state_bucket_name"),
        ("fn backend_bucket() { \"state\" }\n", "backend_bucket()"),
    ] {
        let backend_source =
            format!("backend s3 {{\n  bucket = {bucket_expression}\n  key = \"state.json\"\n}}\n");
        let diagnostics =
            backend_directory_diagnostics_with_sibling(sibling_source, &backend_source);
        assert!(
            diagnostics.iter().all(|diagnostic| {
                !diagnostic.message.contains("backend type 's3'")
                    && !diagnostic.message.contains("attribute 'bucket'")
            }),
            "{bucket_expression} diagnostics: {diagnostics:#?}"
        );
        assert_backend_typo_is_diagnosed(sibling_source, &backend_source);
    }
}

#[test]
fn failed_backend_user_function_does_not_emit_a_schema_type_mismatch() {
    let sibling_source = "fn backend_bucket(): String { true }\n";
    let backend_source = "backend s3 {\n  bucket = backend_bucket()\n  key = \"state.json\"\n}\n";

    let diagnostics = backend_directory_diagnostics_with_sibling(sibling_source, backend_source);

    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("Invalid attribute 'bucket'")
                && !diagnostic.message.contains("expected String")
        }),
        "diagnostics: {diagnostics:#?}"
    );
    assert_backend_typo_is_diagnosed(sibling_source, backend_source);
}

#[test]
fn retained_backend_function_does_not_hide_other_sorted_attribute_errors() {
    let sibling_source = "fn backend_bucket(): String { true }\n";
    let backend_source = "backend s3 {\n  bucket = backend_bucket()\n  key = \"state.json\"\n  region = true\n  encrypt = \"yes\"\n}\n";

    let diagnostics = backend_directory_diagnostics_with_sibling(sibling_source, backend_source);
    let messages: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("backend type 's3'"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();

    assert_eq!(
        messages,
        vec![
            "Invalid attribute 'encrypt' for backend type 's3': Type mismatch: expected Bool, got String",
            "Invalid attribute 'region' for backend type 's3': Type mismatch: expected Region, got Bool",
        ],
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn nested_retained_backend_function_does_not_emit_interpolation_mismatch() {
    let sibling_source = "fn backend_prefix(): String { true }\n";
    let backend_source =
        "backend s3 {\n  bucket = \"${backend_prefix()}-state\"\n  key = \"state.json\"\n}\n";

    let diagnostics = backend_directory_diagnostics_with_sibling(sibling_source, backend_source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("Invalid attribute 'bucket'")
                && !diagnostic.message.contains("expected String")
        }),
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn backend_resource_reference_reports_schema_type_mismatch() {
    let diagnostics = backend_directory_diagnostics_with_sibling(
        "let b = aws.s3.Bucket { bucket_name = \"state\" }\n",
        "backend s3 {\n  bucket = b.bucket_name\n  key = \"k\"\n}\n",
    );

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Invalid attribute 'bucket'")
                && diagnostic.message.contains("expected String")
                && diagnostic.message.contains("ResourceRef")
        }),
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn backend_builtin_call_with_resource_ref_reports_schema_type_mismatch() {
    let diagnostics = backend_directory_diagnostics_with_sibling(
        "let vpc = aws.ec2.Vpc { cidr_block = \"10.0.0.0/16\" }\n",
        "backend s3 {\n  bucket = upper(vpc.vpc_id)\n  key = \"k\"\n}\n",
    );

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Invalid attribute 'bucket'")
                && diagnostic.message.contains("expected String")
                && diagnostic.message.contains("FunctionCall")
        }),
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn backend_schema_check_skips_directories_with_multiple_backend_blocks() {
    let valid = "backend s3 {\n  bucket = \"state\"\n  key = \"k\"\n}\n";
    let invalid = "backend s3 {\n  bucket = \"state\"\n  key = \"k\"\n  typo = true\n}\n";

    for (open_buffer, sibling) in [(valid, invalid), (invalid, valid)] {
        let diagnostics = backend_directory_diagnostics_with_sibling(sibling, open_buffer);
        assert!(
            diagnostics.iter().all(|diagnostic| {
                !diagnostic.message.contains("backend type 's3'")
                    && !diagnostic.message.contains("Unknown attribute 'typo'")
            }),
            "open buffer:\n{open_buffer}\nsibling:\n{sibling}\ndiagnostics: {diagnostics:#?}"
        );
    }

    let diagnostics = backend_directory_diagnostics_with_sibling("\n", invalid);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Unknown attribute 'typo'")),
        "single-backend positive-control diagnostics: {diagnostics:#?}"
    );
}

fn render_value_type_error(
    engine: &DiagnosticEngine,
    type_expr: &TypeExpr,
    value: &Value,
) -> Option<String> {
    let result = match engine.provider_context.loaded() {
        Some(provider_context) => carina_core::validation::typecheck::check_type_expr_value(
            type_expr,
            value,
            provider_context,
            carina_core::validation::typecheck::TypecheckInference::Unavailable,
        ),
        None => carina_core::validation::typecheck::check_type_expr_value_providerless(
            type_expr,
            value,
            &engine.provider_context,
            carina_core::validation::typecheck::TypecheckInference::Unavailable,
        ),
    };
    result.err().map(|error| error.to_string())
}

fn unused_binding_names(diagnostics: &[Diagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .filter_map(|diagnostic| {
            diagnostic
                .message
                .strip_prefix("Unused let binding '")
                .and_then(|rest| rest.split_once('\''))
                .map(|(binding, _)| binding.to_string())
        })
        .collect()
}

fn authored_binding_regression_schemas() -> SchemaRegistry {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("label", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("items", AttributeType::list(AttributeType::string()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "config",
                    AttributeType::struct_(
                        "Config",
                        vec![StructField::optional("label", AttributeType::string()).build()],
                    ),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("arn", AttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

#[test]
fn prevent_destroy_non_literal_uses_the_shared_parser_warning_in_lsp() {
    let engine = custom_engine(authored_binding_regression_schemas());
    let source = r#"provider mock {}
let allow_destroy = false
mock.test.resource {
  name = "example"
  directives { prevent_destroy = allow_destroy }
}
"#;

    let diagnostics = engine.analyze(&create_document(source), None);

    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("only a literal `false` opts out of schema-default protection")
        })
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        warnings[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(warnings[0].range.start.line, 4);
}

#[test]
fn unused_binding_analysis_matches_validate_for_authored_values() {
    struct Case {
        label: &'static str,
        same_file: bool,
        usage: &'static str,
        function_source: Option<&'static str>,
        expected: &'static [&'static str],
    }

    let cases = [
        Case {
            label: "static attribute in map",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  tags = { Source = bucket.name }
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "top-level interpolation",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "${bucket.name}.example.com"
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "interpolation inside list",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  items = ["${bucket.name}-list"]
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "interpolation inside map",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  tags = { Source = "${bucket.name}-map" }
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "interpolation inside struct",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  config = { label = "${bucket.name}-struct" }
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "provider-populated attribute",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  label = bucket.arn
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "export function call",
            same_file: false,
            usage: r#"exports {
  x: String = upper(bucket.name)
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "attribute function call",
            same_file: false,
            usage: r#"attributes {
  x: String = upper(bucket.name)
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "resource function call across files",
            same_file: false,
            usage: r#"mock.test.resource {
  name = upper(bucket.name)
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "sibling user function call on resource reference",
            same_file: false,
            usage: r#"mock.test.resource {
  name = ident(bucket.name)
}
"#,
            function_source: Some("fn ident(v: String): String { v }\n"),
            expected: &["orphan"],
        },
        Case {
            label: "sibling user function ignoring resource reference",
            same_file: false,
            usage: r#"mock.test.resource {
  name = label(bucket.name)
}
"#,
            function_source: Some("fn label(v: String): String { \"fixed\" }\n"),
            expected: &["orphan"],
        },
        Case {
            label: "resource function call in same file",
            same_file: true,
            usage: r#"mock.test.resource {
  name = upper(bucket.name)
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "resource map function call",
            same_file: false,
            usage: r#"mock.test.resource {
  name = "consumer"
  tags = { A = upper(bucket.name) }
}
"#,
            function_source: None,
            expected: &["orphan"],
        },
        Case {
            label: "never used",
            same_file: false,
            usage: "# bucket intentionally unused\n",
            function_source: None,
            expected: &["bucket", "orphan"],
        },
    ];

    let schemas = authored_binding_regression_schemas();
    let engine = custom_engine(schemas.clone());
    let binding = r#"let bucket = mock.test.resource {
  name = "repro"
}

let orphan = mock.test.resource {
  name = "orphan"
}
"#;
    let mut failures = Vec::new();

    for case in cases {
        let fixture = tempfile::tempdir().unwrap();
        let main = if case.same_file {
            format!("{binding}\n{}", case.usage)
        } else {
            binding.to_string()
        };
        let usage = if case.same_file {
            "# usage is in main.crn\n"
        } else {
            case.usage
        };
        std::fs::write(fixture.path().join("main.crn"), &main).unwrap();
        std::fs::write(fixture.path().join("usage.crn"), usage).unwrap();
        if let Some(function_source) = case.function_source {
            std::fs::write(fixture.path().join("functions.crn"), function_source).unwrap();
        }

        let path = fixture.path().join("main.crn");
        let diagnostics = engine.analyze(&create_document_at(&main, path.clone()), Some(&path));
        let lsp_unused = unused_binding_names(&diagnostics);
        let loaded = carina_core::config_loader::load_configuration_with_config(
            fixture.path(),
            &carina_core::parser::ProviderContext::default(),
            &schemas,
        )
        .unwrap();
        let validate_unused =
            carina_core::validation::check_unused_bindings_with_identity_requirements(
                &loaded.authored_bindings,
                &schemas,
            );
        let expected = case
            .expected
            .iter()
            .map(|binding| (*binding).to_string())
            .collect::<Vec<_>>();

        if lsp_unused != validate_unused || lsp_unused != expected {
            failures.push(format!(
                "{}: LSP={lsp_unused:?}, validate={validate_unused:?}, expected={expected:?}",
                case.label
            ));
        }
    }

    for (label, source, expected) in [
        (
            "no-base-path resource function call",
            format!("{binding}\nmock.test.resource {{\n  name = upper(bucket.name)\n}}\n"),
            vec!["orphan".to_string()],
        ),
        (
            "no-base-path unused control",
            binding.to_string(),
            vec!["bucket".to_string(), "orphan".to_string()],
        ),
    ] {
        let diagnostics = engine.analyze(&create_document(&source), None);
        let lsp_unused = unused_binding_names(&diagnostics);
        if lsp_unused != expected {
            failures.push(format!(
                "{label}: LSP={lsp_unused:?}, expected={expected:?}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "unused-binding parity failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn failed_directory_parse_does_not_emit_buffer_only_unused_binding_warning() {
    let schemas = authored_binding_regression_schemas();
    let engine = custom_engine(schemas);
    let fixture = tempfile::tempdir().unwrap();
    let main = r#"let bucket = mock.test.resource {
  name = "bucket"
}

let orphan = mock.test.resource {
  name = "orphan"
}
"#;
    let consumer = r#"mock.test.resource {
  name = bucket.name
}
"#;
    std::fs::write(fixture.path().join("main.crn"), main).unwrap();
    std::fs::write(fixture.path().join("consumer.crn"), consumer).unwrap();
    std::fs::write(fixture.path().join("broken.crn"), "let broken = {\n").unwrap();

    let directory_parse =
        carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
            fixture.path(),
            &carina_core::parser::ProviderContext::default(),
            &std::collections::HashMap::new(),
        );
    assert!(
        directory_parse.is_err(),
        "malformed directory fixture unexpectedly parsed",
    );

    let path = fixture.path().join("main.crn");
    let diagnostics = engine.analyze(&create_document_at(main, path.clone()), Some(&path));
    let unused = unused_binding_names(&diagnostics);

    assert!(
        unused.iter().all(|binding| binding != "bucket"),
        "diagnostics: {diagnostics:#?}",
    );
    assert!(
        unused.iter().all(|binding| binding != "orphan"),
        "diagnostics: {diagnostics:#?}",
    );
}

#[test]
fn attribute_derived_bindings_required_across_sibling_files_are_not_unused() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let alpha = "let alpha = mock.test.resource {\n  name = \"alpha\"\n}\n";
    let beta = "let beta = mock.test.resource {\n  name = \"beta\"\n}\n";
    std::fs::write(temp.path().join("alpha.crn"), alpha).unwrap();
    std::fs::write(temp.path().join("beta.crn"), beta).unwrap();

    let alpha_path = temp.path().join("alpha.crn");
    let beta_path = temp.path().join("beta.crn");
    let alpha_diagnostics = engine.analyze(
        &create_document_at(alpha, alpha_path.clone()),
        Some(&alpha_path),
    );
    let beta_diagnostics = engine.analyze(
        &create_document_at(beta, beta_path.clone()),
        Some(&beta_path),
    );

    assert!(
        unused_binding_names(&alpha_diagnostics).is_empty(),
        "alpha diagnostics: {alpha_diagnostics:#?}",
    );
    assert!(
        unused_binding_names(&beta_diagnostics).is_empty(),
        "beta diagnostics: {beta_diagnostics:#?}",
    );
}

#[test]
fn import_target_in_sibling_state_block_is_not_unused() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource").attribute(
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let resource = "let imported = mock.test.resource {\n  name = \"x\"\n}\n";
    let state_block = r#"provider mock {}

import {
  to = mock.test.resource 'imported'
  id = 'existing-1'
}
"#;
    std::fs::write(temp.path().join("resources.crn"), resource).unwrap();
    std::fs::write(temp.path().join("state.crn"), state_block).unwrap();

    let path = temp.path().join("resources.crn");
    let diagnostics = engine.analyze(&create_document_at(resource, path.clone()), Some(&path));

    assert!(
        unused_binding_names(&diagnostics).is_empty(),
        "import target diagnostics: {diagnostics:#?}",
    );
}

#[test]
fn single_unused_attribute_derived_binding_still_warns() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = "let alpha = mock.test.resource {\n  name = \"alpha\"\n}\n";
    let diagnostics = engine.analyze(&create_document(source), None);

    assert_eq!(
        unused_binding_names(&diagnostics),
        vec!["alpha".to_string()],
    );
}

#[test]
fn unused_stable_binding_still_warns() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource").attribute(
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = "let alpha = mock.test.resource {\n  name = \"alpha\"\n}\n";
    let diagnostics = engine.analyze(&create_document(source), None);

    assert_eq!(
        unused_binding_names(&diagnostics),
        vec!["alpha".to_string()],
    );
}

#[test]
fn attribute_derived_anonymous_identity_error_is_directory_scoped_and_anchored() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let alpha = "mock.test.resource {\n  name = \"alpha\"\n}\n";
    let beta = "\nmock.test.resource {\n  name = \"beta\"\n}\n";
    std::fs::write(base.join("alpha.crn"), alpha).unwrap();
    std::fs::write(base.join("beta.crn"), beta).unwrap();

    let alpha_path = base.join("alpha.crn");
    let beta_path = base.join("beta.crn");
    let alpha_diagnostics = engine.analyze(
        &create_document_at(alpha, alpha_path.clone()),
        Some(&alpha_path),
    );
    let beta_diagnostics = engine.analyze(
        &create_document_at(beta, beta_path.clone()),
        Some(&beta_path),
    );

    for (diagnostics, expected_line) in [(&alpha_diagnostics, 0), (&beta_diagnostics, 1)] {
        let matching = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message == ATTRIBUTE_DERIVED_IDENTITY_ERROR)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(matching[0].range.start.line, expected_line);
        assert_eq!(matching[0].range.start.character, 0);
        assert_eq!(matching[0].range.end.line, expected_line);
        assert_eq!(matching[0].range.end.character, 18);
    }
}

#[test]
fn attribute_derived_anonymous_identity_error_anchors_every_same_file_declaration() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let source = "mock.test.resource {\n  name = \"alpha\"\n}\n\nmock.test.resource {\n  name = \"beta\"\n}\n";
    std::fs::write(temp.path().join("main.crn"), source).unwrap();

    let path = temp.path().join("main.crn");
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("identity is derived from mutable attributes")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].range.start.line, 0);
    assert_eq!(matching[1].range.start.line, 4);
}

#[test]
fn attribute_derived_identity_diagnostics_ignore_for_generated_resources_when_anchoring() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let source = r#"let generated = for name in ["one", "two"] {
  mock.test.resource {
    name = name
  }
}

mock.test.resource {
  name = "p"
}

mock.test.resource {
  name = "q"
}
"#;
    std::fs::write(temp.path().join("main.crn"), source).unwrap();

    let path = temp.path().join("main.crn");
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("identity is derived from mutable attributes")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].range.start.line, 6);
    assert_eq!(matching[1].range.start.line, 10);
}

#[test]
fn attribute_derived_identity_diagnostics_ignore_commented_resource_blocks() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let temp = tempfile::tempdir().unwrap();
    let source = r#"# mock.test.resource {
#   name = "commented"
# }

mock.test.resource {
  name = "p"
}

mock.test.resource {
  name = "q"
}
"#;
    std::fs::write(temp.path().join("main.crn"), source).unwrap();

    let path = temp.path().join("main.crn");
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("identity is derived from mutable attributes")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].range.start.line, 4);
    assert_eq!(matching[1].range.start.line, 8);
}

#[test]
fn missing_provider_schema_suppresses_attribute_derived_identity_conflict() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let source = r#"awscc.iam.role {
  role_name = "one"
}

awscc.iam.role {
  role_name = "two"
}
"#;
    let diagnostics = engine.analyze(&create_document(source), None);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("has no loaded schema")),
        "the existing missing-schema diagnostic must still fire: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("identity is derived from mutable attributes")),
        "a missing schema must not also produce the identity-basis diagnostic: {diagnostics:#?}"
    );
}

#[test]
fn block_name_not_flagged_as_unknown() {
    let engine = test_engine();
    // Use operating_region (singular block_name) instead of operating_regions
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
name = "test-ipam"
operating_region {
    region_name = "ap-northeast-1"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown attribute 'operating_region'"));
    assert!(
        unknown.is_none(),
        "block_name 'operating_region' should not be flagged as unknown. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Create a DiagnosticEngine with a schema that has deeply nested structs for testing.
#[test]
fn unknown_field_in_nested_struct_block() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        unknown_nested = "bad"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'unknown_nested'"));
    assert!(
        unknown.is_some(),
        "Should warn about unknown field in nested struct block. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn type_mismatch_in_nested_struct_field() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        leaf_int = "not_a_number"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("Int"));
    assert!(
        mismatch.is_some(),
        "Should warn about type mismatch in nested struct field. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        mismatch.unwrap().severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING),
        "ordinary nested validation failures must retain warning severity"
    );
}

#[test]
fn module_placeholder_leaf_does_not_hide_invalid_struct_sibling() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let tags = AttributeType::struct_(
        "Tags".to_string(),
        vec![
            StructField::optional("Name", AttributeType::string()).build(),
            StructField::optional("Count", AttributeType::int()).build(),
        ],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::optional("tags", tags).build())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let source = r#"arguments { var_name: String }
mock.test.Thing {
  tags = {
    Name = var_name
    Count = "not-an-int"
  }
}
"#;

    let parsed = carina_core::parser::parse_file(
        source,
        std::path::Path::new("module.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("module source parses");
    let core_error = carina_core::validation::validate_resources(
        &parsed,
        &schemas,
        &std::collections::HashSet::from(["mock".to_string()]),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect_err("core validation rejects Count");
    assert!(
        core_error.contains("Count") && core_error.contains("Int"),
        "unexpected core diagnostic: {core_error}",
    );

    let diagnostics = custom_engine(schemas).analyze(&create_document(source), None);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("Int")
                && diagnostic.range.start.line == 4
        }),
        "LSP must retain validate parity for the concrete sibling: {diagnostics:#?}",
    );
}

#[test]
fn valid_nested_struct_no_diagnostics() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        leaf_field = "valid"
        leaf_int = 42
    }
    outer_field = "also valid"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    // Filter to only struct-related diagnostics (ignore unknown attribute warnings from test schema)
    let struct_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Unknown field") || d.message.contains("Type mismatch"))
        .collect();
    assert!(
        struct_diags.is_empty(),
        "Valid nested struct should have no field diagnostics. Got: {:?}",
        struct_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn read_only_nested_struct_field_diagnostic_is_anchored_on_offending_line() {
    let engine = test_engine_with_nested_structs();
    let doc = create_document(
        r#"let r = test.nested.resource {
outer {
    inner {
        provider_leaf = "cannot-set-this"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let read_only = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message
                == "Attribute 'provider_leaf' is populated by the provider and cannot be set"
        })
        .collect::<Vec<_>>();

    assert_eq!(
        read_only.len(),
        1,
        "expected one nested read-only diagnostic: {diagnostics:#?}"
    );
    assert_eq!(read_only[0].range.start.line, 3);
    assert_eq!(read_only[0].range.start.character, 8);
    assert_eq!(read_only[0].range.end.line, 3);
    assert_eq!(read_only[0].range.end.character, 21);
    assert_eq!(
        read_only[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "nested read-only assignments must have the same severity as top-level ones"
    );
}

#[test]
fn read_only_shadowed_arn_diagnostics_land_on_top_level_and_nested_lines() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let config = AttributeType::struct_(
        "Config".to_string(),
        vec![StructField::provider_populated("arn", AttributeType::string()).build()],
    );
    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::provider_populated("arn", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("config", config).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"mock.test.resource {
  arn = "top-level-provider-arn"
  config = {
    arn = "nested-provider-arn"
  }
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let mut read_only = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message == "Attribute 'arn' is populated by the provider and cannot be set"
        })
        .collect::<Vec<_>>();
    read_only.sort_by_key(|diagnostic| diagnostic.range.start.line);

    assert_eq!(
        read_only.len(),
        2,
        "top-level and nested arn assignments need independent diagnostics: {diagnostics:#?}"
    );
    assert_eq!(
        read_only
            .iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert!(
        read_only.iter().all(|diagnostic| diagnostic.severity
            == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)),
        "both writability diagnostics must be errors: {read_only:#?}"
    );
}

#[test]
fn read_only_field_inside_map_struct_is_anchored_at_the_map_field() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let entry = AttributeType::struct_(
        "Entry".to_string(),
        vec![StructField::provider_populated("arn", AttributeType::string()).build()],
    );
    let config = AttributeType::struct_(
        "Config".to_string(),
        vec![StructField::optional("entries", AttributeType::map(entry)).build()],
    );
    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::optional("config", config).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"mock.test.resource {
  config = {
    entries = {
      primary = {
        arn = "provider-arn"
      }
    }
  }
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("Attribute 'arn' is populated by the provider and cannot be set")
        })
        .unwrap_or_else(|| panic!("expected map writability diagnostic: {diagnostics:#?}"));

    assert_eq!(
        diagnostic.range.start.line, 2,
        "Map collection currently anchors nested errors at the map field"
    );
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "a coarser Map anchor must not downgrade a writability failure"
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn enum_invalid_value_top_level() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
instance_tenancy = awscc.ec2.Vpc.InstanceTenancy.invalid_value
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_value"));
    assert!(
        enum_diag.is_some(),
        "Should warn about invalid Enum value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn enum_valid_value_top_level() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
instance_tenancy = awscc.ec2.Vpc.InstanceTenancy.default
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("instance_tenancy") && d.message.contains("invalid"));
    assert!(
        enum_diag.is_none(),
        "Should NOT warn about valid Enum value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn enum_invalid_value_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.SecurityGroup.IpProtocol.invalid_proto
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_proto"));
    assert!(
        enum_diag.is_some(),
        "Should warn about invalid Enum value in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn enum_valid_value_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.SecurityGroup.IpProtocol.tcp
    from_port = 80
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let enum_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ip_protocol") && d.message.contains("invalid"));
    assert!(
        enum_diag.is_none(),
        "Should NOT warn about valid Enum value in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn custom_type_validation_in_struct_field() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
group_description = "Test security group"
security_group_ingress {
    ip_protocol = awscc.ec2.SecurityGroup.IpProtocol.tcp
    from_port = 99999
    to_port = 80
    cidr_ip = "0.0.0.0/0"
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let port_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("from_port") || d.message.contains("99999"));
    assert!(
        port_diag.is_some(),
        "Should warn about out-of-range port in struct field. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn provider_custom_type_failures_emit_once_at_the_attribute() {
    let engine = test_engine_with_suspended_validation_provider();

    for (value, expected_message) in [
        ("invalid-input", "rejected custom-type value"),
        ("first-time-timeout", "first custom-type timeout"),
    ] {
        let doc = create_document(&format!(
            r#"mock.test.resource {{
  probe = "{value}"
}}
"#,
        ));

        let diagnostics = engine.analyze(&doc, None);
        assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
        let diagnostic = &diagnostics[0];
        assert!(
            diagnostic.message.contains(expected_message),
            "diagnostic: {diagnostic:#?}"
        );
        assert_eq!(
            diagnostic.range.start.line, 1,
            "diagnostic: {:#?}",
            diagnostic
        );
        assert_eq!(
            diagnostic.range.start.character, 2,
            "diagnostic: {:#?}",
            diagnostic
        );
    }
}

#[test]
fn provider_custom_type_failure_from_static_secret_points_to_the_attribute() {
    let engine = test_engine_with_suspended_validation_provider();
    let doc = create_document(
        r#"mock.test.resource {
  probe = secret("invalid-input")
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let masked = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("(secret) failed validation"))
        .collect::<Vec<_>>();

    assert_eq!(masked.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(masked[0].range.start.line, 1);
    assert_eq!(masked[0].range.start.character, 2);
}

#[test]
fn suspended_provider_validation_is_reported_once_per_provider_per_pass() {
    let engine = test_engine_with_suspended_validation_provider();
    let doc = create_document(
        r#"let first = mock.test.resource {
  probe = "same"
}
let second = mock.test.resource {
  probe = "same"
}
let third = mock.test.resource {
  probe = "same"
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let suspended = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains(
                "provider validation is suspended after repeated wall-clock timeouts for this input",
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(suspended.len(), 1, "diagnostics: {diagnostics:#?}");
    assert!(
        suspended[0].message.contains("validate_custom_type"),
        "diagnostic must retain the original validation operation: {:#?}",
        suspended[0]
    );
}

#[test]
fn distinct_suspended_inputs_are_each_reported_once_per_pass() {
    let engine = test_engine_with_suspended_validation_provider();
    let doc = create_document(
        r#"let first = mock.test.resource {
  probe = "suspended-a"
}
let second = mock.test.resource {
  probe = "suspended-b"
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let suspended = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains(
                "provider validation is suspended after repeated wall-clock timeouts for this input",
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(suspended.len(), 2, "diagnostics: {diagnostics:#?}");
}

#[test]
fn suspended_provider_config_and_custom_type_inputs_are_both_reported() {
    let engine = test_engine_with_suspended_validation_provider();
    let doc = create_document(
        r#"provider mock {
  region = "suspended-config"
}
let example = mock.test.resource {
  probe = "suspended-custom-type"
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let suspended = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains(
                "provider validation is suspended after repeated wall-clock timeouts for this input",
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(suspended.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        suspended[0].range.start.line, 1,
        "diagnostics: {diagnostics:#?}"
    );
    assert_eq!(
        suspended[1].range.start.line, 4,
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn ordinary_timeouts_are_not_hidden_by_suspension_deduplication() {
    let engine = test_engine_with_suspended_validation_provider();
    let doc = create_document(
        r#"let ordinary_a = mock.test.resource {
  probe = "ordinary-a"
}
let ordinary_b = mock.test.resource {
  probe = "ordinary-b"
}
let unmarked_lookalike = mock.test.resource {
  probe = "unmarked-lookalike"
}
let suspended_a = mock.test.resource {
  probe = "suspended"
}
let suspended_b = mock.test.resource {
  probe = "suspended"
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();

    assert!(
        messages
            .iter()
            .any(|message| message.contains("ordinary timeout for input a")),
        "diagnostics: {diagnostics:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("ordinary timeout for input b")),
        "diagnostics: {diagnostics:#?}"
    );
    assert!(
        messages
            .iter()
            .filter(|message| message.contains("provider validation is suspended"))
            .count()
            > 1,
        "the unmarked lookalike must not be hidden with the marked suspension: {diagnostics:#?}"
    );
}

#[test]
fn suspension_deduplication_does_not_turn_validation_errors_into_success() {
    let engine = test_engine_with_suspended_validation_provider();
    let value = carina_core::resource::Value::<carina_core::resource::NoPlaceholder>::Concrete(
        carina_core::resource::ConcreteValue::String("suspended".into()),
    );
    let attributes = std::collections::HashMap::from([("probe".to_string(), value)]);
    let schema = engine
        .schemas
        .get(
            "mock",
            "test.resource",
            carina_core::schema::SchemaKind::Resource,
        )
        .expect("suspended-validation schema must be registered");
    let validate = || {
        schema
            .validate_with_resolver(
                &attributes,
                &|_| false,
                engine.provider_context.custom_type_resolver(),
            )
            .expect_err("validation must remain an error")
            .into_iter()
            .find(|error| error.host_cause().is_some())
            .expect("provider validation error must retain host policy metadata")
    };

    let first = validate();
    let second = validate();
    let Some(carina_core::provider::HostErrorCause::ValidationSuspended(first_id)) =
        first.host_cause()
    else {
        panic!("first error must retain its suspended-input id: {first:?}");
    };
    let Some(carina_core::provider::HostErrorCause::ValidationSuspended(second_id)) =
        second.host_cause()
    else {
        panic!("second error must retain its suspended-input id: {second:?}");
    };
    assert_eq!(first_id, second_id);

    let deduplicator = ProviderValidationDeduplicator::default();
    assert!(deduplicator.should_emit_type_error(&first));
    assert!(!deduplicator.should_emit_type_error(&second));
}

#[test]
fn enum_declared_custom_type_reaches_provider_in_canonical_api_spelling() {
    let (engine, seen) = test_engine_with_enum_declared_custom_type();
    let doc = create_document(
        r#"let consumer = mock.test.consumer {
  mode = mock.shared.Mode.enabled
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("enum identifier")
                && !diagnostic.message.contains("canonical enum API spelling")
        }),
        "diagnostics: {diagnostics:#?}"
    );
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|value| value == "ENABLED"),
        "seen: {seen:?}"
    );
}

#[test]
fn list_item_type_validation() {
    // Use a test engine with a List(Enum) schema to test list item validation
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let list_enum = AttributeType::list(AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Protocol"),
        Some(vec!["tcp".to_string(), "udp".to_string()]),
        vec![],
        None,
        None,
    ));

    let schema = ResourceSchema::new("list.resource")
        .attribute(AttributeSchema::optional("protocols", list_enum).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["test".to_string()]);

    // Use bare identifiers (`tcp`, `invalid_protocol`) rather than
    // quoted strings — under carina#2986 strict mode, an enum attribute
    // requires the identifier shape. The list is otherwise valid and
    // the `invalid_protocol` item should surface as
    // `InvalidEnumVariant`.
    let doc = create_document(
        r#"let r = test.list.resource {
protocols = [tcp, invalid_protocol]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let item_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("invalid_protocol"));
    assert!(
        item_diag.is_some(),
        "Should warn about invalid item in List(Enum). Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn block_name_mixed_syntax_error() {
    let engine = test_engine();
    // Use both operating_region and operating_regions - should error
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.ipam {
name = "test-ipam"
operating_region {
    region_name = "ap-northeast-1"
}
operating_regions = [{
    region_name = "us-east-1"
}]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let mixed_error = diagnostics.iter().find(|d| {
        d.message.contains("operating_region")
            && d.message.contains("operating_regions")
            && d.message.contains("same attribute")
    });
    assert!(
        mixed_error.is_some(),
        "Should error on mixed block_name and canonical name. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_static_non_member_preserves_enum_diagnostic() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::union(vec![
                    AttributeType::enum_(
                        carina_core::schema::TypeIdentity::bare("Mode"),
                        Some(vec!["active".to_string(), "passive".to_string()]),
                        vec![],
                        None,
                        None,
                    ),
                    AttributeType::int(),
                ]),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = "invalid_mode"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    // No Union member accepts this value, so failure attribution retains the
    // old shape-based route through the Enum arm and its useful variant list.
    let invalid_value_error = diagnostics
        .iter()
        .find(|d| d.message.contains("expects an enum identifier"))
        .filter(|d| d.message.contains("active, passive"));
    assert!(
        invalid_value_error.is_some(),
        "Should preserve the Enum member diagnostic. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_enum_and_struct_preserves_enum_quick_fix_diagnostic() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    };
    use tower_lsp::lsp_types::DiagnosticSeverity;

    let schema = ResourceSchema::new("test.resource").attribute(
        AttributeSchema::optional(
            "mode",
            AttributeType::union(vec![
                AttributeType::enum_(
                    TypeIdentity::bare("Mode"),
                    Some(vec!["active".to_string(), "passive".to_string()]),
                    vec![],
                    None,
                    None,
                ),
                AttributeType::struct_(
                    "ModeOptions",
                    vec![StructField::optional("custom", AttributeType::string()).build()],
                ),
            ]),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let source = concat!(
        "provider test {}\n\n",
        "test.test.resource {\n",
        "  mode = \"invalid_mode\"\n",
        "}\n",
    );

    let diagnostics = custom_engine(schemas).analyze(&create_document(source), None);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("invalid_mode")
                && diagnostic.severity == Some(DiagnosticSeverity::WARNING)
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    let diagnostic = matching[0];
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(3, 9), Position::new(3, 23))
    );
    assert_eq!(
        diagnostic
            .data
            .as_ref()
            .and_then(|data| data.get("tag"))
            .and_then(serde_json::Value::as_str),
        Some("carina_enum_mismatch")
    );
}

#[test]
fn tied_scalar_union_diagnostic_is_directory_scoped_and_matches_core() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, enum_identity,
    };

    let hosted_zone_id = AttributeType::union(vec![
        AttributeType::enum_(
            enum_identity(
                "CloudFrontHostedZoneId",
                Some("test.route53.Record.AliasTarget"),
            ),
            Some(vec!["global".to_string()]),
            vec![],
            None,
            None,
        ),
        AttributeType::refined_string(
            Some(TypeIdentity::new(
                Some("test"),
                ["route53", "Record", "AliasTarget"],
                "Route53HostedZoneId",
            )),
            Some("^Z[A-Z0-9]+$".to_string()),
            None,
            None,
        ),
    ]);
    let schema = ResourceSchema::new("route53.Record")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("hosted_zone_id", hosted_zone_id).build())
        .validate()
        .expect("test schema must be valid");
    let attributes = std::collections::HashMap::from([
        (
            "name".to_string(),
            Value::<carina_core::resource::NoPlaceholder>::Concrete(ConcreteValue::String(
                "record".into(),
            )),
        ),
        (
            "hosted_zone_id".to_string(),
            Value::Concrete(ConcreteValue::String("z14grhdcwa56qt".into())),
        ),
    ]);
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(SchemaFixtureFactory {
        name: "test".to_string(),
        schemas: vec![schema.clone()],
    })];
    let core_resolver =
        carina_core::provider::provider_custom_types_from_factories(Arc::new(factories));
    let mut schemas = SchemaRegistry::new();
    schemas.insert("test", schema);
    let core_errors = schemas
        .get(
            "test",
            "route53.Record",
            carina_core::schema::SchemaKind::Resource,
        )
        .expect("test schema must be owner-bound")
        .validate_with_resolver(
            &attributes,
            &|name| name == "hosted_zone_id",
            &core_resolver,
        )
        .expect_err("core validation must reject both union alternatives");
    assert_eq!(core_errors.len(), 1, "core errors: {core_errors:#?}");
    let core_message = core_errors[0].to_string();
    assert!(
        core_message.contains("CloudFrontHostedZoneId.global")
            && core_message.contains("/^Z[A-Z0-9]+$/"),
        "core must render both tied alternatives: {core_message}"
    );

    let engine = custom_engine(schemas);
    let fixture = tempfile::tempdir().expect("multi-file diagnostics fixture");
    let main = r#"test.route53.Record {
  name = "record"
  hosted_zone_id = "z14grhdcwa56qt"
}
"#;
    std::fs::write(fixture.path().join("main.crn"), main).expect("write main.crn");
    std::fs::write(fixture.path().join("providers.crn"), "provider test {}\n")
        .expect("write providers.crn");

    let path = fixture.path().join("main.crn");
    let diagnostics = engine.analyze(&create_document_at(main, path.clone()), Some(&path));
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message == core_message)
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "LSP must report the core union diagnostic exactly once: {diagnostics:#?}"
    );
    assert_eq!(matching[0].range.start.line, 2);
    let uri = tower_lsp::lsp_types::Url::parse("file:///tmp/main.crn").unwrap();
    let actions = crate::code_action::code_actions_for_diagnostic(&uri, matching[0]);
    let enum_action = actions
        .iter()
        .find(|action| {
            action.title
                == "Replace with `test.route53.Record.AliasTarget.CloudFrontHostedZoneId.global`"
        })
        .unwrap_or_else(|| {
            panic!("the enum arm's quick-fix must survive the Union diagnostic: {actions:#?}")
        });
    assert_eq!(enum_action.is_preferred, Some(false));
}

#[test]
fn union_attribute_with_deferred_resource_ref_emits_no_diagnostic() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, enum_identity};

    let source = ResourceSchema::new("source.Item")
        .attribute(AttributeSchema::optional("value", AttributeType::string()).build());
    let sink = ResourceSchema::new("sink.Item").attribute(
        AttributeSchema::optional(
            "target",
            AttributeType::union(vec![
                AttributeType::enum_(
                    enum_identity("TargetKind", None),
                    Some(vec!["known".to_string()]),
                    vec![],
                    None,
                    None,
                ),
                AttributeType::refined_string(None, Some("^value-".to_string()), None, None),
            ]),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"let source = test.source.Item {
  value = "value-from-source"
}

test.sink.Item {
  target = source.value
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics.is_empty(),
        "a deferred ResourceRef must not be validated as a concrete Union value: {diagnostics:#?}"
    );
}

#[test]
fn resource_scalar_string_mismatches_use_core_value_descriptions_once() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build())
        .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
        .attribute(AttributeSchema::optional("ratio", AttributeType::float()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"mock.test.resource {
  enabled = "yes"
  count   = "forty-two"
  ratio   = "three-point-one-four"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("Type mismatch: expected"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();

    let expected = [
        "Type mismatch: expected Bool, got string 'yes'. Use true or false.",
        "Type mismatch: expected Int, got string 'forty-two'.",
        "Type mismatch: expected Float, got string 'three-point-one-four'.",
    ];
    assert_eq!(
        mismatches.len(),
        expected.len(),
        "diagnostics: {diagnostics:#?}"
    );
    for expected in expected {
        assert_eq!(
            mismatches
                .iter()
                .filter(|message| **message == expected)
                .count(),
            1,
            "each scalar mismatch should be reported once: {diagnostics:#?}"
        );
    }
}

#[test]
fn resource_scalar_mismatches_never_display_environment_plaintext() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build())
        .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
        .attribute(AttributeSchema::optional("ratio", AttributeType::float()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let variable = "CARINA_LSP_ATTRIBUTE_SECRET";
    let plaintext = "hunter2SECRET";
    unsafe {
        std::env::set_var(variable, plaintext);
    }
    let doc = create_document(&format!(
        r#"mock.test.resource {{
  enabled = env('{variable}')
  count   = env('{variable}')
  ratio   = env('{variable}')
}}"#
    ));

    let diagnostics = engine.analyze(&doc, None);
    unsafe {
        std::env::remove_var(variable);
    }
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("Type mismatch: expected"))
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 3, "diagnostics: {diagnostics:#?}");
    assert!(
        mismatches
            .iter()
            .all(|diagnostic| diagnostic.message.contains("got string")),
        "diagnostics: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains(plaintext)),
        "environment plaintext leaked: {diagnostics:#?}"
    );
}

#[test]
fn union_of_structs_reports_alternatives_at_the_attribute() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let retention = AttributeType::union(vec![
        AttributeType::struct_(
            "RetentionDays".to_string(),
            vec![StructField::required("days", AttributeType::int()).build()],
        ),
        AttributeType::struct_(
            "RetentionYears".to_string(),
            vec![StructField::required("years", AttributeType::int()).build()],
        ),
    ]);
    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("retention", retention).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
retention = {
days = 30
years = 1
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let matching = diagnostics
        .iter()
        .filter(|d| {
            d.message == "Expected exactly one of 'days' or 'years', but both were supplied"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "LSP must surface exactly one union-level diagnostic. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diagnostic = matching[0];
    assert_eq!(
        diagnostic.range.start.line, 6,
        "the union diagnostic must be anchored at the retention attribute"
    );
}

#[test]
fn union_valid_static_value_no_warning() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::union(vec![
                    AttributeType::enum_(
                        carina_core::schema::TypeIdentity::bare("Mode"),
                        Some(vec!["active".to_string(), "passive".to_string()]),
                        vec![],
                        None,
                        None,
                    ),
                    AttributeType::int(),
                ]),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = "active"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_error = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_error.is_none(),
        "Should NOT warn about valid Union value. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn union_valid_int_value_no_warning() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::union(vec![
                    AttributeType::enum_(
                        carina_core::schema::TypeIdentity::bare("Mode"),
                        Some(vec!["active".to_string(), "passive".to_string()]),
                        vec![],
                        None,
                        None,
                    ),
                    AttributeType::int(),
                ]),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"provider test {
region = "ap-northeast-1"
}

test.test.resource {
name = "my-resource"
mode = 42
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_error = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_error.is_none(),
        "Should NOT warn when Int value matches Union member. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_undefined_binding_reference() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    sg_id: String = nonexistent.id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("nonexistent"));
    assert!(
        undefined_diag.is_some(),
        "Should warn about undefined binding in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_valid_binding_reference() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
group_description = "Test security group"
}

attributes {
    sg_id: String = sg.group_id
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("sg"));
    assert!(
        undefined_diag.is_none(),
        "Should NOT warn about defined binding in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_type_mismatch_bool_to_string() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    flag: String = true
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected String, got bool"));
    assert!(
        type_diag.is_some(),
        "Should warn about type mismatch in attributes block (bool assigned to string). Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_valid_types_no_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let sg = awscc.ec2.SecurityGroup {
group_description = "Test security group"
}

attributes {
    sg_id: String = sg.group_id
    name: String = "hello"
    enabled: Bool = true
    count: Int = 42
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected") && d.message.contains("got"));
    assert!(
        type_diag.is_none(),
        "Should NOT warn about valid types in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn nested_block_name_not_flagged_as_unknown() {
    // When a nested struct field uses block_name (e.g., "transition" for "transitions"),
    // validate_struct_value should not flag it as an unknown field.
    let engine = test_engine_with_block_name_nested();
    let doc = create_document(
        r#"let r = test.block.resource {
config = {
    transition {
        days = 30
        storage_class = "GLACIER"
    }
}
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown field 'transition'"));
    assert!(
        unknown.is_none(),
        "block_name 'transition' should not be flagged as unknown in nested struct. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn find_let_binding_position_with_multibyte_leading_whitespace() {
    // Regression test for issue #724: find_let_binding_position uses byte offset
    // as character column. When multi-byte whitespace (e.g., full-width space U+3000)
    // appears before "let", the byte offset differs from the character offset.
    let engine = test_engine();

    // U+3000 (ideographic space) is 3 bytes in UTF-8 but 1 character.
    // Rust's str::trim() strips it as Unicode whitespace.
    // Line: "\u{3000}let my_var = awscc.ec2.Vpc { }"
    // "let " starts at byte 3, but character offset 1.
    // name_col should be char 1 + 4 = 5 (correct)
    // Bug produces byte 3 + 4 = 7 (wrong)
    let text = "\u{3000}let my_var = awscc.ec2.Vpc { }";
    let result = engine.find_let_binding_position(text, "my_var");
    assert_eq!(
        result,
        Some((0, 5)),
        "Column should be character offset (5), not byte offset (7)"
    );
}

#[test]
fn attributes_block_detection_with_brace_on_same_line() {
    // Regression test: ensure output block detection works correctly
    // after removing the redundant `|| trimmed == "attributes {"` condition.
    // The simplified condition `starts_with("output ") && contains('{')` must
    // still detect `attributes {` (the only valid output block syntax).
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    flag: String = true
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("expected String, got bool"));
    assert!(
        type_diag.is_some(),
        "Output block detection should work with simplified condition. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn undefined_reference_detected_for_non_id_name_properties() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
name = "test-vpc"
cidr_block = "10.0.0.0/16"
}

awscc.ec2.Subnet {
name = "test-subnet"
vpc_id = nonexistent_vpc.vpc_id
cidr_block = "10.0.1.0/24"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("nonexistent_vpc"));
    assert!(
        undefined_diag.is_some(),
        "Should warn about undefined reference 'nonexistent_vpc.vpc_id'. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn defined_reference_not_flagged_for_non_id_name_properties() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let main_vpc = awscc.ec2.Vpc {
name = "test-vpc"
cidr_block = "10.0.0.0/16"
}

awscc.ec2.Subnet {
name = "test-subnet"
vpc_id = main_vpc.vpc_id
cidr_block = "10.0.1.0/24"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") && d.message.contains("main_vpc"));
    assert!(
        undefined_diag.is_none(),
        "Should NOT warn about defined binding 'main_vpc'. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn provider_in_module_emits_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"arguments {
    vpc_cidr: String
}

provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
    cidr_block = args.vpc_cidr
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let provider_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("provider blocks are not allowed inside modules")
    });
    assert!(
        provider_diag.is_some(),
        "Should error about provider block in module. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = provider_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diag.range.start.line, 4);
    assert_eq!(diag.range.start.character, 0);
}

#[test]
fn provider_in_module_with_sibling_arguments_emits_error() {
    // Issue #3717 repro shape: the open buffer contains only the provider,
    // while a sibling file carries the marker that makes the directory a
    // module. Buffer-only classification misses this combination.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("arguments.crn"),
        r#"arguments {
    name: String = "test"
}
"#,
    )
    .unwrap();
    std::fs::write(base.join("providers.crn"), "provider mock { }\n").unwrap();

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join("providers.crn")).unwrap();
    let path = base.join("providers.crn");
    let doc = create_document_at(&buffer, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let provider_diag = diagnostics.iter().find(|diagnostic| {
        diagnostic
            .message
            .contains("provider blocks are not allowed inside modules")
    });
    assert!(
        provider_diag.is_some(),
        "Should error when a sibling file marks the directory as a module. Got: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
    let diag = provider_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diag.range.start.line, 0);
    assert_eq!(diag.range.start.character, 0);
}

#[test]
fn arguments_buffer_with_sibling_provider_has_no_provider_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let arguments = r#"arguments {
    name: String = "test"
}
"#;
    std::fs::write(base.join("arguments.crn"), arguments).unwrap();
    std::fs::write(base.join("providers.crn"), "provider mock { }\n").unwrap();

    let engine = test_engine();
    let path = base.join("arguments.crn");
    let doc = create_document_at(arguments, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));
    let provider_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("provider blocks are not allowed inside modules")
        })
        .collect();

    assert!(
        provider_diagnostics.is_empty(),
        "a provider diagnostic must only anchor in the buffer that declares it. Got: {provider_diagnostics:?}",
    );
}

#[test]
fn provider_in_root_shaped_directory_no_error() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(base.join("providers.crn"), "provider mock { }\n").unwrap();
    std::fs::write(base.join("values.crn"), "let environment = \"test\"\n").unwrap();

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join("providers.crn")).unwrap();
    let path = base.join("providers.crn");
    let doc = create_document_at(&buffer, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("provider blocks are not allowed inside modules")),
        "Should not reject a provider when no module marker exists. Got: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn moved_block_in_module_emits_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"arguments {
    name: String = "test"
}

moved {
    from = mock.test.Resource 'old'
    to = mock.test.Resource 'new'
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let state_block_diag = diagnostics.iter().find(|diagnostic| {
        diagnostic
            .message
            .contains("state blocks (moved, removed, and import) are not allowed inside modules")
    });
    assert!(
        state_block_diag.is_some(),
        "Should error about a moved block in a module. Got diagnostics: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
    let diag = state_block_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diag.range.start.line, 4);
    assert_eq!(diag.range.start.character, 0);
}

#[test]
fn state_blocks_in_module_with_sibling_arguments_emit_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("arguments.crn"),
        r#"arguments {
    name: String = "test"
}
"#,
    )
    .unwrap();
    let state_blocks = r#"moved {
    from = mock.test.Resource 'old'
    to = mock.test.Resource 'new'
}

removed {
    from = mock.test.Resource 'legacy'
}

import {
    to = mock.test.Resource 'existing'
    id = 'resource-123'
}
"#;
    std::fs::write(base.join("state.crn"), state_blocks).unwrap();

    let engine = test_engine();
    let path = base.join("state.crn");
    let doc = create_document_at(state_blocks, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));
    let state_block_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains(
                "state blocks (moved, removed, and import) are not allowed inside modules",
            )
        })
        .collect();

    assert_eq!(
        state_block_diagnostics.len(),
        3,
        "all three state block variants should be diagnosed. Got: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
    assert!(
        state_block_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
        "state block diagnostics must be errors: {state_block_diagnostics:?}",
    );
    assert_eq!(
        state_block_diagnostics
            .iter()
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>(),
        vec![0, 5, 9],
    );
}

#[test]
fn arguments_buffer_with_sibling_state_block_has_no_state_block_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let arguments = r#"arguments {
    name: String = "test"
}
"#;
    std::fs::write(base.join("arguments.crn"), arguments).unwrap();
    std::fs::write(
        base.join("state.crn"),
        "removed {\n    from = mock.test.Resource 'legacy'\n}\n",
    )
    .unwrap();

    let engine = test_engine();
    let path = base.join("arguments.crn");
    let doc = create_document_at(arguments, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));
    let state_block_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains(
                "state blocks (moved, removed, and import) are not allowed inside modules",
            )
        })
        .collect();

    assert!(
        state_block_diagnostics.is_empty(),
        "a state block diagnostic must only anchor in the buffer that declares it. Got: {state_block_diagnostics:?}",
    );
}

#[test]
fn state_blocks_in_root_shaped_directory_no_error() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let state_blocks = r#"moved {
    from = mock.test.Resource 'old'
    to = mock.test.Resource 'new'
}

removed {
    from = mock.test.Resource 'legacy'
}

import {
    to = mock.test.Resource 'existing'
    id = 'resource-123'
}
"#;
    std::fs::write(base.join("state.crn"), state_blocks).unwrap();
    std::fs::write(base.join("values.crn"), "let environment = \"test\"\n").unwrap();

    let engine = test_engine();
    let path = base.join("state.crn");
    let doc = create_document_at(state_blocks, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("state blocks (moved, removed, and import) are not allowed inside modules")),
        "Should not reject root state blocks when no module marker exists. Got: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn discarded_upstream_state_does_not_suppress_real_module_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"let _ = upstream_state { source = './a' }
arguments {
    n: String = "x"
}
let up = upstream_state { source = './b' }
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let upstream_state_lines: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("upstream_state declarations are not allowed inside modules")
        })
        .map(|diagnostic| diagnostic.range.start.line)
        .collect();

    assert_eq!(
        upstream_state_lines,
        vec![4],
        "the discarded declaration must not hide or receive the real module violation: {diagnostics:#?}",
    );
}

#[test]
fn arguments_with_backend_emits_error() {
    // Issue #2198. `arguments` is a module-input declaration. A `backend`
    // block is root-only, so the combination is unambiguously a misplaced
    // `arguments` block in a root configuration.
    let engine = test_engine();
    let doc = create_document(
        r#"arguments {
    state_path: String = "state.json"
}

backend local {
    path = arguments.state_path
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let arguments_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("arguments blocks are only valid inside module definitions")
    });
    assert!(
        arguments_diag.is_some(),
        "Should error about arguments block in root config. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = arguments_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn secret_derived_backend_attribute_matches_cli_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"backend local {
    path = secret("carina.state.json")
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("backend local attribute 'path'")
                && diagnostic.message.contains("plain string")
                && diagnostic.message.contains("secret()")
        })
        .unwrap_or_else(|| {
            panic!(
        "LSP must surface the same backend secret rejection as validate/plan: {diagnostics:#?}",
            )
        });
    assert_eq!(diagnostic.range.start.line, 1);
    assert_eq!(diagnostic.range.start.character, 4);
    assert_eq!(diagnostic.range.end.line, 1);
    assert_eq!(diagnostic.range.end.character, 8);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("carina.state.json")),
        "LSP diagnostic must not echo secret-derived plaintext: {diagnostics:#?}",
    );
}

#[test]
fn arguments_with_sibling_backend_emits_error() {
    // Issue #2198 repro shape: arguments live in main.crn, backend lives
    // in backend.crn. The directory-scoped parse must merge them and the
    // diagnostic must still fire — single-file inspection would miss it.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("main.crn"),
        r#"arguments {
    state_path: String = "state.json"
}
"#,
    )
    .unwrap();
    std::fs::write(
        base.join("backend.crn"),
        r#"backend local {
    path = arguments.state_path
}
"#,
    )
    .unwrap();

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let path = base.join("main.crn");
    let doc = create_document_at(&buffer, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    assert!(
        diagnostics.iter().any(|d| d
            .message
            .contains("arguments blocks are only valid inside module definitions")),
        "Should error about arguments block when sibling file declares a backend. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn arguments_without_backend_no_error() {
    // A directory with only `arguments` (no `backend`/`provider`) is
    // indistinguishable from a module being authored, so the LSP must
    // not flag it. The CLI's root-config check at validate time still
    // catches the case where such a directory is loaded as a root.
    let engine = test_engine();
    let doc = create_document(
        r#"arguments {
    vpc_cidr: String
}

awscc.ec2.Vpc {
    cidr_block = args.vpc_cidr
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let arguments_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("arguments blocks are only valid inside module definitions")
    });
    assert!(
        arguments_diag.is_none(),
        "Should NOT flag arguments block when no backend is present. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn provider_without_module_markers_no_error() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let provider_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("provider blocks are not allowed inside modules")
    });
    assert!(
        provider_diag.is_none(),
        "Should NOT error about provider in non-module file. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn unknown_function_call_produces_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
    cidr_block = not_a_function("hello")
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let func_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function 'not_a_function'"));
    assert!(
        func_diag.is_some(),
        "Should report unknown function 'not_a_function'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let diag = func_diag.unwrap();
    assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn known_function_call_no_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
    cidr_block = join("-", ["a", "b"])
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let func_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function"));
    assert!(
        func_diag.is_none(),
        "Known function 'join' should not produce unknown function diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires provider schemas"]
fn resource_ref_typo_suggests_similar_attribute() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let igw = awscc.ec2.internet_gateway {
}

let rt = awscc.ec2.RouteTable {
  vpc_id = "vpc-123"
}

awscc.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw.internet_gateway_idd
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let typo_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("Unknown attribute 'internet_gateway_idd'")
            && d.message.contains("Did you mean 'internet_gateway_id'?")
    });
    assert!(
        typo_diag.is_some(),
        "Should warn about typo in resource ref attribute with suggestion. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_valid_attribute_no_warning() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let igw = awscc.ec2.internet_gateway {
}

awscc.ec2.vpc_gateway_attachment {
  internet_gateway_id = igw.internet_gateway_id
  vpc_id              = "vpc-123"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let ref_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown attribute") && d.message.contains("igw"));
    assert!(
        ref_diag.is_none(),
        "Valid attribute reference should not produce warning. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn pipe_preferred_direct_call_produces_info_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let name = join("-", parts)
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let pipe_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Consider using pipe form for 'join'"));
    assert!(
        pipe_diag.is_some(),
        "Direct call to pipe-preferred function should produce diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = pipe_diag.unwrap();
    assert_eq!(
        diag.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::INFORMATION),
        "Pipe-preferred diagnostic should be info-level, not warning"
    );
}

#[test]
fn shared_typecheck_ipv4_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv4_address".to_string());
    let value = Value::Concrete(ConcreteValue::String("not-an-ip".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv4_address"
    );
}

#[test]
fn shared_typecheck_ipv4_address_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv4_address".to_string());
    let value = Value::Concrete(ConcreteValue::String("192.168.1.1".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv4_address. Got: {:?}",
        result
    );
}

#[test]
fn shared_typecheck_ipv6_cidr_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string());
    let value = Value::Concrete(ConcreteValue::String("not-a-cidr".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv6_cidr"
    );
}

#[test]
fn shared_typecheck_ipv6_cidr_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string());
    let value = Value::Concrete(ConcreteValue::String("2001:db8::/32".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_cidr. Got: {:?}",
        result
    );
}

#[test]
fn shared_typecheck_ipv6_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_address".to_string());
    let value = Value::Concrete(ConcreteValue::String("not-an-ipv6".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv6_address"
    );
}

#[test]
fn shared_typecheck_ipv6_address_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::Simple("ipv6_address".to_string());
    let value = Value::Concrete(ConcreteValue::String("2001:db8::1".into()));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_address. Got: {:?}",
        result
    );
}

#[test]
fn shared_typecheck_list_ipv4_address_invalid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::List(Box::new(
        carina_core::parser::TypeExpr::Simple("ipv4_address".to_string()),
    ));
    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("192.168.1.1".into())),
        Value::Concrete(ConcreteValue::String("bad-ip".into())),
    ]));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_some(),
        "Should return error for invalid ipv4_address in list"
    );
    assert!(
        result.as_ref().unwrap().contains("list element 1"),
        "Error should reference element index. Got: {:?}",
        result
    );
}

#[test]
fn shared_typecheck_list_ipv6_cidr_valid() {
    let engine = test_engine();
    let type_expr = carina_core::parser::TypeExpr::List(Box::new(
        carina_core::parser::TypeExpr::Simple("ipv6_cidr".to_string()),
    ));
    let value = Value::Concrete(ConcreteValue::List(vec![
        Value::Concrete(ConcreteValue::String("2001:db8::/32".into())),
        Value::Concrete(ConcreteValue::String("::/0".into())),
    ]));
    let result = render_value_type_error(&engine, &type_expr, &value);
    assert!(
        result.is_none(),
        "Should not return error for valid ipv6_cidr list. Got: {:?}",
        result
    );
}

#[test]
fn pipe_preferred_pipe_form_no_diagnostic() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

let name = parts |> join("-")
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let pipe_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("Consider using pipe form for 'join'"));
    assert!(
        pipe_diag.is_none(),
        "Pipe form should not produce pipe-preferred diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv4_address_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    ip: Ipv4Address = "not-an-ip"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv4_address") || d.message.contains("IPv4"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv4_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv4_address_valid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    ip: Ipv4Address = "192.168.1.1"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv4_address") || d.message.contains("IPv4"));
    assert!(
        type_diag.is_none(),
        "Should NOT warn about valid ipv4_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_cidr_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    network: Ipv4Cidr = "not-a-cidr"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("CIDR") || d.message.contains("cidr"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid cidr in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv6_address_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    addr: Ipv6Address = "not-ipv6"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("ipv6") || d.message.contains("IPv6"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv6_address in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_type_check_helper_regression() {
    // Regression test for refactoring: all three ResourceRef type-checking paths
    // (Union, Enum, Custom) must produce consistent "Type mismatch" diagnostics.
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn dummy_validate(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(s),
            ) if s.expose_plaintext().starts_with("test.") => Ok(()),
            _ => Err("invalid custom value".to_string()),
        }
    }

    // Source resource: has a String attribute "name" and a Custom "my_id"
    let source_schema = ResourceSchema::new("source")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "my_id",
                AttributeType::enum_(
                    carina_core::schema::enum_identity("MyId", Some("test")),
                    None,
                    vec![],
                    Some(legacy_validator(dummy_validate)),
                    None,
                ),
            )
            .build(),
        );

    // Target resource: has Union, Enum, and Custom attributes
    let target_schema = ResourceSchema::new("target")
        .attribute(
            AttributeSchema::optional(
                "union_attr",
                AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "enum_attr",
                AttributeType::enum_(
                    carina_core::schema::TypeIdentity::bare("Status"),
                    Some(vec!["active".to_string(), "inactive".to_string()]),
                    vec![],
                    None,
                    None,
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "custom_attr",
                AttributeType::enum_(
                    carina_core::schema::enum_identity("MyId", Some("test")),
                    None,
                    vec![],
                    Some(legacy_validator(dummy_validate)),
                    None,
                ),
            )
            .build(),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        target_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    // Case 1: Union attr with incompatible ResourceRef (MyId != Int|Bool) -> mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
union_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let union_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("MyId"));
    assert!(
        union_mismatch.is_some(),
        "Union attr should warn about type mismatch for incompatible ResourceRef. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 2: Enum attr with incompatible ResourceRef (MyId != Status) -> mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
enum_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let enum_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("MyId"));
    assert!(
        enum_mismatch.is_some(),
        "Enum attr should warn about type mismatch for incompatible ResourceRef. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 3: Custom attr with compatible ResourceRef (MyId == MyId) -> no mismatch
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
custom_attr = src.my_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let custom_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("custom_attr"));
    assert!(
        custom_mismatch.is_none(),
        "Custom attr should NOT warn when ResourceRef type matches. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // Case 4: Union attr with String ResourceRef -> no mismatch (String is always compatible)
    let doc = create_document(
        r#"let src = test.source {
name = "hello"
}

test.target {
union_attr = src.name
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let string_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch") && d.message.contains("union_attr"));
    assert!(
        string_mismatch.is_none(),
        "Union attr should NOT warn when ResourceRef is String type. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn attributes_block_ipv6_cidr_invalid() {
    let engine = test_engine();
    let doc = create_document(
        r#"provider awscc {
region = awscc.Region.ap_northeast_1
}

attributes {
    net6: Ipv6Cidr = "not-a-cidr"
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let type_diag = diagnostics
        .iter()
        .find(|d| d.message.contains("IPv6") || d.message.contains("ipv6"));
    assert!(
        type_diag.is_some(),
        "Should warn about invalid ipv6_cidr in attributes block. Got diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_validation_failed_with_attribute_points_to_attribute_line() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, ValidatorType};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        )
        .with_declared_validators(vec![ValidatorType::TagsKeyValueCheck]);

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let doc = create_document(
        "mock.test.resource {\n  name = 'test'\n  tags = {\n    key = 'Project'\n    value = 'carina'\n  }\n}",
    );

    let diagnostics = engine.analyze(&doc, None);
    let tags_diag = diagnostics.iter().find(|d| {
        d.message
            .contains("tags map contains both 'key' and 'value'")
    });
    assert!(
        tags_diag.is_some(),
        "Should have a tags validation error. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = tags_diag.unwrap();
    // The diagnostic should point to the "tags" line (line 2, 0-indexed),
    // not the resource declaration line (line 0).
    assert_eq!(
        diag.range.start.line, 2,
        "Diagnostic should point to the 'tags' attribute line (line 2), not the resource line. Got line {}",
        diag.range.start.line
    );
}

#[test]
fn builtin_cidr_validator_diagnostic_points_to_attribute_line() {
    use carina_core::schema::{AttributeSchema, ResourceSchema, types};

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", types::ipv4_cidr()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document("awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/33'\n}");

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("prefix length"))
        .unwrap_or_else(|| panic!("expected invalid CIDR diagnostic: {diagnostics:#?}"));
    assert_eq!(
        diagnostic.range.start.line, 1,
        "CIDR validator diagnostic must point to cidr_block: {diagnostic:#?}"
    );
}

fn read_only_attribute_engine(attribute: carina_core::schema::AttributeSchema) -> DiagnosticEngine {
    let schema = carina_core::schema::ResourceSchema::new("test.resource").attribute(attribute);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    custom_engine(schemas)
}

fn assert_only_read_only_diagnostic<'a>(
    diagnostics: &'a [tower_lsp::lsp_types::Diagnostic],
    attribute: &str,
) -> &'a tower_lsp::lsp_types::Diagnostic {
    assert_eq!(
        diagnostics.len(),
        1,
        "'{attribute}' must produce exactly one diagnostic, got: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
    let diagnostic = &diagnostics[0];
    assert_eq!(
        diagnostic.message,
        format!("Attribute '{attribute}' is populated by the provider and cannot be set")
    );
    diagnostic
}

#[test]
fn read_only_wrong_typed_value_reports_only_writability() {
    use carina_core::schema::{AttributeSchema, AttributeType};

    let engine = read_only_attribute_engine(
        AttributeSchema::provider_populated("enabled", AttributeType::bool()).build(),
    );
    let doc = create_document("mock.test.resource {\n  enabled = 'not-a-bool'\n}\n");

    let diagnostics = engine.analyze(&doc, None);
    assert_only_read_only_diagnostic(&diagnostics, "enabled");
}

#[test]
fn read_only_invalid_enum_reports_no_enum_quick_fix() {
    use carina_core::schema::{AttributeSchema, AttributeType};

    let mode = AttributeType::enum_(
        carina_core::schema::enum_identity("Mode", Some("mock.test.resource")),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    let engine =
        read_only_attribute_engine(AttributeSchema::provider_populated("mode", mode).build());
    let doc = create_document("mock.test.resource {\n  mode = 'bogus'\n}\n");

    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = assert_only_read_only_diagnostic(&diagnostics, "mode");
    assert!(
        diagnostic.data.is_none(),
        "read-only enum diagnostics must not carry enum replacement data"
    );
    let uri = tower_lsp::lsp_types::Url::parse("file:///tmp/main.crn").unwrap();
    assert!(
        crate::code_action::code_actions_for_diagnostic(&uri, diagnostic).is_empty(),
        "read-only enum assignments must not offer replacement quick-fixes"
    );
}

#[test]
fn read_only_list_struct_reports_no_shape_or_nested_diagnostics() {
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let setting = AttributeType::struct_(
        "Setting".to_string(),
        vec![StructField::required("enabled", AttributeType::bool()).build()],
    );
    let engine = read_only_attribute_engine(
        AttributeSchema::provider_populated("settings", AttributeType::list(setting)).build(),
    );
    let doc = create_document(
        "mock.test.resource {\n  settings = [{\n    enabled = 'not-a-bool'\n    unexpected = true\n  }]\n}\n",
    );

    let diagnostics = engine.analyze(&doc, None);
    assert_only_read_only_diagnostic(&diagnostics, "settings");
}

#[test]
fn shared_definition_assignment_is_accepted_only_through_its_writable_path() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let schema = ResourceSchema::new("test.resource")
        .with_def(
            "Tag",
            AttributeType::struct_(
                "Tag",
                vec![StructField::required("key", AttributeType::string()).build()],
            ),
        )
        .attribute(
            AttributeSchema::optional("tags", AttributeType::list(AttributeType::ref_("Tag")))
                .build(),
        )
        .attribute(
            AttributeSchema::provider_populated(
                "effective_tags",
                AttributeType::list(AttributeType::ref_("Tag")),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("shared definition must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        "mock.test.resource {\n  tags { key = 'writable' }\n  effective_tags = [{ key = 'read-only' }]\n}\n",
    );

    let diagnostics = engine.analyze(&doc, None);
    assert_only_read_only_diagnostic(&diagnostics, "effective_tags");
}

#[test]
fn read_only_assignment_points_to_attribute_line() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("test.resource")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::provider_populated("region", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let doc = create_document("mock.test.resource {\n  name = 'test'\n  region = 'us-west-2'\n}\n");
    let diagnostics = engine.analyze(&doc, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.message
                == "Attribute 'region' is populated by the provider and cannot be set"
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a read-only assignment diagnostic, got: {:?}",
                diagnostics
                    .iter()
                    .map(|diagnostic| &diagnostic.message)
                    .collect::<Vec<_>>()
            )
        });

    assert_eq!(
        diagnostic.range.start.line, 2,
        "read-only diagnostic must point to the attribute, not the resource header"
    );
    assert_eq!(diagnostic.range.start.character, 2);
}

#[test]
fn warning_when_provider_loaded_but_schema_missing() {
    // Provider is loaded but doesn't have a schema for this resource type.
    // Should show WARNING (not ERROR), not "Unknown resource type".
    let engine = DiagnosticEngine::new(
        loaded_test_providers(SchemaRegistry::new()),
        vec!["awscc".to_string()],
    );
    let doc = create_document(
        r#"awscc.iam.role {
  role_name = 'test'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let unknown_type = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown resource type"));
    assert!(
        unknown_type.is_none(),
        "Loaded provider should not show 'Unknown resource type'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    let no_schema = diagnostics
        .iter()
        .find(|d| d.message.contains("No schema for"));
    assert!(
        no_schema.is_some(),
        "Should show 'No schema' warning. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        no_schema.unwrap().severity,
        Some(DiagnosticSeverity::WARNING)
    );
}

#[test]
fn error_when_provider_has_no_loaded_schema_for_file_configuration() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"awscc.iam.role {
  role_name = 'test'
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let expected = "Provider 'awscc' has no loaded schema for this file: no provider block for it was found in this file's configuration.";
    let unavailable = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == expected);
    assert!(
        unavailable.is_some(),
        "a provider absent from this file's loaded configuration should be distinguished from an unknown resource type. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        unavailable.unwrap().severity,
        Some(DiagnosticSeverity::ERROR)
    );
    assert!(
        !unavailable.unwrap().message.contains("carina init"),
        "a missing-schema diagnostic must not suggest initialization"
    );

    // And the old generic message should no longer fire for this case.
    let generic_unknown = diagnostics
        .iter()
        .find(|d| d.message == "Unknown resource type: awscc.iam.role");
    assert!(
        generic_unknown.is_none(),
        "Should not emit generic 'Unknown resource type' when the provider is absent from the file's loaded configuration. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn export_inference_without_loaded_providers_does_not_report_provider_checked_failure() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document("exports {\n  value = missing.attribute\n}\n");

    let diagnostics = engine.analyze(&doc, None);

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("type annotation required")),
        "the no-provider path must not mint provider-schema inference evidence: {diagnostics:#?}",
    );
}

#[test]
fn no_undefined_resource_for_namespaced_enum_value() {
    // provider_names includes "awscc", so awscc.xxx.yyy.EnumType.VALUE
    // should NOT be flagged as "Undefined resource"
    let engine = DiagnosticEngine::new(
        loaded_test_providers(SchemaRegistry::new()),
        vec!["awscc".to_string()],
    );
    let doc = create_document(
        r#"awscc.organizations.organization {
  feature_set = awscc.organizations.organization.FeatureSet.ALL
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Namespaced enum value should not be flagged as undefined resource. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn no_undefined_resource_for_declared_but_uninstalled_provider() {
    // Regression for #2019: when a provider is declared (`provider awscc { ... }`)
    // in the current document but hasn't been downloaded (installed provider_names
    // is empty), the provider-namespaced enum reference on its right-hand side
    // used to be flagged as `Undefined resource: 'awscc'. Define it with 'let
    // awscc = aws...'` — which is both wrong (awscc is a namespace, not a let
    // binding) and actively misleading (following the fix breaks valid DSL).
    // Nothing installed — simulates missing .carina/.
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Declared-but-uninstalled provider name must not be flagged as 'Undefined resource'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn undefined_resource_still_fires_when_identifier_is_not_a_declared_provider() {
    // Regression guard for #2019: the fix must not swallow legitimate undefined-
    // binding diagnostics. When the root identifier is not a declared provider
    // and not a defined binding, the existing "Undefined resource" message
    // should still fire.
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"provider awscc {
  region = totally_unknown.some_attr
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource: 'totally_unknown'"));
    assert!(
        undefined.is_some(),
        "A genuinely unknown identifier should still produce the 'Undefined resource' diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn map_key_validation_warns_on_invalid_key() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    // Build schema: statement.condition has Enum keys
    let condition_type = AttributeType::map_with_key(
        AttributeType::enum_(
            carina_core::schema::TypeIdentity::bare("ConditionOperator"),
            Some(vec!["string_equals".to_string(), "string_like".to_string()]),
            vec![],
            None,
            None,
        ),
        AttributeType::map(AttributeType::string()),
    );
    let statement_type = AttributeType::struct_(
        "Statement".to_string(),
        vec![
            StructField::optional("effect", AttributeType::string()).build(),
            StructField::optional("condition", condition_type).build(),
        ],
    );
    let schema = ResourceSchema::new("test.resource").attribute(
        AttributeSchema::optional(
            "policy",
            AttributeType::struct_(
                "Policy".to_string(),
                vec![
                    StructField::optional("statement", AttributeType::list(statement_type)).build(),
                ],
            ),
        )
        .build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );

    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["test".to_string()]);

    // Invalid condition key "unknown_op"
    let doc = create_document(
        r#"test.test.resource {
  policy = {
    statement {
      effect = 'Allow'
      condition = {
        unknown_op = { 'key' = 'value' }
      }
    }
  }
}
"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let has_key_error = diagnostics
        .iter()
        .any(|d| d.message.contains("Map key") || d.message.contains("unknown_op"));
    assert!(
        has_key_error,
        "Should warn about invalid map key 'unknown_op'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn distinct_semantic_customs_are_rejected() {
    // Regression test for #2079: distinct semantic-typed Custom types
    // (AwsAccountId vs TargetId) are NOT assignable. The previous permissive
    // rule (#1795) collapsed all String-based Customs into one compatibility
    // class, which silently accepted `target_id = sso.identity_store_id`.
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    fn validate_account_id(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(s),
            ) if s.expose_plaintext().len() == 12 => Ok(()),
            _ => Err("expected 12-digit account ID".to_string()),
        }
    }

    fn validate_target_id(v: &carina_core::resource::Value) -> Result<(), String> {
        match v {
            carina_core::resource::Value::Concrete(
                carina_core::resource::ConcreteValue::String(_),
            ) => Ok(()),
            _ => Err("expected string".to_string()),
        }
    }

    // Source resource: data source with an AwsAccountId attribute
    let source_schema = ResourceSchema::new("sts.caller_identity")
        .as_data_source()
        .attribute(
            AttributeSchema::optional(
                "account_id",
                AttributeType::refined_string_with_validator(
                    carina_core::schema::TypeIdentity::new(
                        Some("aws"),
                        Vec::<String>::new(),
                        "AwsAccountId",
                    ),
                    None,
                    None,
                    legacy_validator(validate_account_id),
                    None,
                ),
            )
            .build(),
        );

    // Target resource: has a TargetId attribute (also String-based Custom)
    let target_schema = ResourceSchema::new("sso.Assignment").attribute(
        AttributeSchema::optional(
            "target_id",
            AttributeType::refined_string_with_validator(
                carina_core::schema::TypeIdentity::new(
                    Some("awscc"),
                    Vec::<String>::new(),
                    "TargetId",
                ),
                None,
                None,
                legacy_validator(validate_target_id),
                None,
            ),
        )
        .build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        target_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let doc = create_document(
        r#"let caller = read aws.sts.caller_identity {}

awscc.sso.Assignment {
target_id = caller.account_id
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let type_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("Type mismatch"));
    assert!(
        type_mismatch.is_some(),
        "AwsAccountId → TargetId must be rejected (distinct semantic types). Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn resource_ref_type_mismatch_resolves_binding_from_sibling_file() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, legacy_validator,
    };

    let source_schema = ResourceSchema::new("source.Item").attribute(
        AttributeSchema::optional(
            "source_id",
            AttributeType::refined_string_with_validator(
                TypeIdentity::bare("SourceId"),
                None,
                None,
                legacy_validator(|_| Ok(())),
                None,
            ),
        )
        .build(),
    );
    let sink_schema = ResourceSchema::new("sink.Item").attribute(
        AttributeSchema::optional(
            "target_id",
            AttributeType::refined_string_with_validator(
                TypeIdentity::bare("TargetId"),
                None,
                None,
                legacy_validator(|_| Ok(())),
                None,
            ),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("source.crn"),
        "let source_item = test.source.Item {}\n",
    )
    .unwrap();
    let sink = r#"let sink_item = test.sink.Item {
  target_id = source_item.source_id
}
"#;
    std::fs::write(base.join("sink.crn"), sink).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "sink.crn", sink);
    let mismatch = diagnostics.iter().find(|diagnostic| {
        diagnostic.message.contains("Type mismatch")
            && diagnostic.message.contains("TargetId")
            && diagnostic.message.contains("SourceId")
            && diagnostic.message.contains("source_item.source_id")
    });

    assert!(
        mismatch.is_some(),
        "resource ref type checking must resolve schema bindings from sibling files: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn resource_ref_string_matches_ref_membered_sink_union() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let source_schema = ResourceSchema::new("source.Item")
        .attribute(AttributeSchema::optional("value", AttributeType::string()).build());
    let sink_schema = ResourceSchema::new("sink.Item")
        .attribute(
            AttributeSchema::optional(
                "target",
                AttributeType::union(vec![
                    AttributeType::ref_("S".to_string()),
                    AttributeType::ref_("L".to_string()),
                ]),
            )
            .build(),
        )
        .with_def("S", AttributeType::string())
        .with_def("L", AttributeType::list(AttributeType::string()));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"let source = test.source.Item {}

let sink = test.sink.Item {
    target = source.value
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Type mismatch")),
        "Ref-membered sink union should accept the String source: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn resource_ref_mismatch_displays_resolved_ref_type_names() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, legacy_validator,
    };

    let refined = |name: &str| {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare(name),
            None,
            None,
            legacy_validator(|_| Ok(())),
            None,
        )
    };
    let source_schema = ResourceSchema::new("source.Item")
        .attribute(
            AttributeSchema::optional("value", AttributeType::ref_("Source".to_string())).build(),
        )
        .with_def("Source", refined("SourceId"));
    let sink_schema = ResourceSchema::new("sink.Item")
        .attribute(
            AttributeSchema::optional("target", AttributeType::ref_("Target".to_string())).build(),
        )
        .with_def("Target", refined("TargetId"));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"let source = test.source.Item {}

let sink = test.sink.Item {
    target = source.value
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let mismatch = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Type mismatch"))
        .expect("distinct resolved identities should produce a mismatch");
    assert!(
        mismatch.message.contains("expected TargetId, got SourceId"),
        "resolved source and sink names should be displayed: {}",
        mismatch.message,
    );
    assert!(
        !mismatch.message.contains("Ref("),
        "schema-internal Ref names must not leak into the diagnostic: {}",
        mismatch.message,
    );
}

#[test]
fn resource_ref_mismatch_displays_resolved_union_member_names() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let source_schema = ResourceSchema::new("source.Item")
        .attribute(AttributeSchema::optional("value", AttributeType::bool()).build());
    let sink_schema = ResourceSchema::new("sink.Item")
        .attribute(
            AttributeSchema::optional(
                "target",
                AttributeType::union(vec![
                    AttributeType::ref_("S".to_string()),
                    AttributeType::ref_("L".to_string()),
                ]),
            )
            .build(),
        )
        .with_def("S", AttributeType::string())
        .with_def("L", AttributeType::list(AttributeType::string()));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(
        r#"let source = test.source.Item {}

let sink = test.sink.Item {
    target = source.value
}
"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    let mismatch = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Type mismatch"))
        .expect("Bool should be rejected by String | List<String>");
    assert!(
        mismatch
            .message
            .contains("expected String | List<String>, got Bool"),
        "nested Ref members should be resolved for display: {}",
        mismatch.message,
    );
    assert!(
        !mismatch.message.contains("Ref("),
        "schema-internal Ref names must not leak into the diagnostic: {}",
        mismatch.message,
    );
}

#[test]
fn exports_cross_file_ref_no_false_positive() {
    // Regression: when exports.crn references a binding from a sibling file,
    // the parser produces a ResourceRef for `binding.attr`.
    // The custom type validator (e.g., aws_account_id: 12-digit check) must
    // skip ResourceRefs to avoid false positives.
    use carina_core::resource::{ConcreteValue, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let aws_account_id_type = AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("AwsAccountId"),
        None,
        None,
        carina_core::schema::legacy_validator(|v| match v {
            Value::Concrete(ConcreteValue::String(s))
                if s.expose_plaintext().len() == 12
                    && s.expose_plaintext().chars().all(|c| c.is_ascii_digit()) =>
            {
                Ok(())
            }
            Value::Concrete(ConcreteValue::String(s)) => Err(format!(
                "must be exactly 12 digits, got {} characters",
                s.expose_plaintext().len()
            )),
            _ => Err("expected string".to_string()),
        }),
        None,
    );
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::optional("account_id", aws_account_id_type).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let doc = create_document(
        r#"exports {
  accounts: list(AwsAccountId) = [
    registry_prod.account_id,
  ]
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);

    let false_positive = diagnostics
        .iter()
        .find(|d| d.message.contains("12 digits") || d.message.contains("12 characters"));
    assert!(
        false_positive.is_none(),
        "Dot-notation cross-file ref should be skipped by type validator. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_dotted_string_literal_is_not_ref_type_checked() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::optional("account_id", AttributeType::int()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account {\n  name = 'prod'\n}\n",
    )
    .unwrap();
    let exports = "exports {\n  account: String = \"registry_prod.account_id\"\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);

    let ref_type_warning = diagnostics.iter().find(|d| {
        d.message.contains("type mismatch") && d.message.contains("registry_prod.account_id")
    });
    assert!(
        ref_type_warning.is_none(),
        "quoted dotted string literal must not be checked as a ResourceRef. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_survives_formatter_round_trip() {
    use carina_core::formatter::{FormatConfig, format};

    let original = r#"exports {
  values: list(Bool) = [
    "nope",
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();

    let engine = test_engine();
    let before = engine.analyze(&create_document(original), None);
    let after = engine.analyze(&create_document(&formatted), None);

    let before_warning = before
        .iter()
        .find(|d| d.message.contains("expected Bool, got string"))
        .map(|d| d.message.clone());
    let after_warning = after
        .iter()
        .find(|d| d.message.contains("expected Bool, got string"))
        .map(|d| d.message.clone());

    // carina-rs/carina#2586: source list is multi-line, so the
    // formatter preserves the multi-line shape.
    assert_eq!(
        formatted,
        "exports {\n  values: list(Bool) = [\n    'nope',\n  ]\n}\n"
    );
    assert_eq!(before_warning, after_warning);
    assert!(
        after_warning.is_some(),
        "formatter round-trip should not suppress exports type warnings; diagnostics after formatting: {:?}",
        after.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_ref_type_warning_survives_formatter_round_trip() {
    use carina_core::formatter::{FormatConfig, format};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("sample.resource")
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let original = r#"let item = test.sample.resource {
  enabled = true
}

exports {
  values: list(String) = [
    item.enabled,
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();

    let before = engine.analyze(&create_document(original), None);
    let after = engine.analyze(&create_document(&formatted), None);

    let before_warning = before
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());
    let after_warning = after
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());

    // carina-rs/carina#2586: the source list is multi-line, so the
    // formatter preserves the multi-line layout (one element per line,
    // trailing comma, normalized indentation).
    assert_eq!(
        formatted,
        "let item = test.sample.resource {\n  enabled = true\n}\n\nexports {\n  values: list(String) = [\n    item.enabled,\n  ]\n}\n"
    );
    assert_eq!(before_warning, after_warning);
    assert!(
        after_warning.is_some(),
        "formatter round-trip should not suppress exports ref type warnings; diagnostics after formatting: {:?}",
        after.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_ref_type_warning_survives_document_reparse_after_format_edit() {
    use carina_core::formatter::{FormatConfig, format};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("sample.resource")
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let original = r#"let item = test.sample.resource {
  enabled = true
}

exports {
  values: list(String) = [
    item.enabled,
  ]
}"#;
    let formatted = format(original, &FormatConfig::default()).unwrap();
    let mut doc = create_document(original);
    let (last_line, last_char) = document_end_position(original);

    doc.apply_change(TextDocumentContentChangeEvent {
        range: Some(Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: last_line,
                character: last_char,
            },
        }),
        range_length: None,
        text: formatted.clone(),
    });

    let diagnostics = engine.analyze(&doc, None);
    let warning = diagnostics
        .iter()
        .find(|d| d.message.contains("export 'values': type mismatch"))
        .map(|d| d.message.clone());

    assert_eq!(doc.text(), formatted);
    assert!(
        doc.parse_error().is_none(),
        "formatted text should reparse cleanly, got: {:?}",
        doc.parse_error()
    );
    assert!(
        warning.is_some(),
        "format edit + reparse should preserve exports ref type warning; diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_after_format_with_cross_file_ref() {
    // Simulate: file has cross-file ref, formatter collapses list,
    // user then changes type annotation to wrong type.
    // Warning should appear for the type mismatch.
    let engine = test_engine();

    // After format-on-save, list is on one line. User changes string→bool.
    let doc = create_document(
        r#"exports {
  accounts: list(Bool) = [registry_prod.account_id, registry_dev.account_id]
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);

    // The entries parse as ResourceRef values. At minimum, this path
    // must not crash or produce custom-validator false positives.
    // A type mismatch may or may not be caught depending on schema availability.
    eprintln!(
        "diagnostics after format+type-change: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    // No false positive about "12 digits"
    let false_positive = diagnostics
        .iter()
        .find(|d| d.message.contains("12 digits") || d.message.contains("12 characters"));
    assert!(
        false_positive.is_none(),
        "Should not produce 12-digit false positive. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_for_literal_mismatch() {
    let engine = test_engine();
    let doc = create_document(
        r#"exports {
  flag: Bool = 'hello'
}"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    eprintln!(
        "literal mismatch diagnostics: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let warning = diagnostics
        .iter()
        .find(|d| d.message.contains("expected Bool"));
    assert!(
        warning.is_some(),
        "Should warn about bool vs string mismatch. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn orphan_exports_check_builtins_without_calling_unloaded_custom_types() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"exports {
  flag: Bool = 'hello'
  identifiers: list(mock.RecoveryProbe) = ['identifier']
}
attributes {
  identifier: mock.RecoveryProbe = 'identifier'
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("expected Bool, got string")),
        "built-in export checks must remain available: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("provider custom types are not loaded")),
        "custom leaves in orphan buffers must not call the unloaded resolver: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn orphan_export_dotted_unresolved_literal_mismatch_warns_without_loaded_providers() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"exports {
  x: aws.AccountId = 42
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == Some(DiagnosticSeverity::WARNING)
                && diagnostic
                    .message
                    .contains("expected aws.AccountId, got int")
        }),
        "providerless dotted export annotations must retain literal mismatch warnings: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn orphan_attribute_nested_dotted_unresolved_shape_mismatch_warns_without_loaded_providers() {
    let engine = DiagnosticEngine::without_loaded_provider_schemas();
    let doc = create_document(
        r#"attributes {
  x: map(aws.AccountId) = ['a']
}"#,
    );

    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == Some(DiagnosticSeverity::WARNING)
                && diagnostic
                    .message
                    .contains("expected map(aws.AccountId), got list")
        }),
        "providerless nested dotted attribute annotations must retain shape mismatch warnings: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn exports_type_warning_multiline_vs_oneline() {
    let engine = test_engine();
    // Multi-line (before format)
    let doc_multi = create_document("exports {\n  flag: Bool = 'hello'\n}");
    let diag_multi = engine.analyze(&doc_multi, None);
    eprintln!(
        "multi-line: {:?}",
        diag_multi.iter().map(|d| &d.message).collect::<Vec<_>>()
    );

    // After user types but before format - with wrong type and literal
    let doc_literal =
        create_document("exports {\n  accounts: list(Bool) = ['literal1', 'literal2']\n}");
    let diag_literal = engine.analyze(&doc_literal, None);
    eprintln!(
        "literal list(Bool): {:?}",
        diag_literal.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_map_type_warning_for_cross_file_ref() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    // Schema: registry_prod.account_id is String — incompatible with map(bool)
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::optional("account_id", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account { name = 'prod' }\nlet registry_dev = awscc.organizations.account { name = 'dev' }\n",
    )
    .unwrap();
    let exports = "exports {\n  accounts: map(Bool) = {\n    prod = registry_prod.account_id\n    dev  = registry_dev.account_id\n  }\n}";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);

    let type_warning = diagnostics
        .iter()
        .find(|d| d.message.contains("type mismatch") || d.message.contains("expected"));
    assert!(
        type_warning.is_some(),
        "Should warn about map(Bool) vs String account_id. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_subscripted_cross_file_ref_does_not_false_flag() {
    // Counterpart to the bare-attribute test above: when an exports
    // value is a `ResourceRef` with a `field_path` or `subscripts`
    // (e.g. `accounts['k']` against a sibling-file binding), the LSP
    // must not compare the receiver type against the head's full
    // attribute type — the subscript narrows the position and a
    // positional walker (#2475) is the precise tool. Today the LSP
    // skips the comparison rather than false-flag, matching the
    // behaviour of the core `check_ref_against_type` after #2447.
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("organizations.account").attribute(
        AttributeSchema::optional("accounts", AttributeType::map(AttributeType::string())).build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let orgs = awscc.organizations.account { name = 'orgs' }\n",
    )
    .unwrap();
    // Receiver expects String. Pre-#2447 (or with naive ResourceRef
    // handling), the LSP would compare `map(String)` vs `String` and
    // emit a spurious mismatch; with subscript-awareness it must stay
    // silent.
    let exports = "exports {\n  dev: String = orgs.accounts['registry_dev']\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);

    let type_warning = diagnostics
        .iter()
        .find(|d| d.message.contains("type mismatch") || d.message.contains("expected"));
    assert!(
        type_warning.is_none(),
        "Subscripted ref must not be compared against head's outer type. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_referencing_module_call_attribute_does_not_require_annotation() {
    // #2493: an `exports` value of the form `<module_call>.<attr>` (e.g.
    // `role_arn = github_actions_carina.role_arn` where
    // `let github_actions_carina = github { ... }` invokes a module)
    // must not surface a `type annotation required: unknown binding`
    // warning. The LSP's directory-merged parse runs module expansion,
    // so inference sees the call as a `Virtual` composition and derives
    // the declared attribute through that post-expansion surface.
    let engine = DiagnosticEngine::new(
        loaded_test_providers(SchemaRegistry::new()),
        vec!["awscc".to_string()],
    );
    let tmp = tempfile::tempdir().unwrap();
    let module_dir = tmp.path().join("github_module");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  name: String\n}\nattributes {\n  role_arn: String = name\n}\n",
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let github = use { source = '../github_module' }\nlet github_actions_carina = github { name = 'demo' }\n",
    )
    .unwrap();
    let exports = "exports {\n  role_arn = github_actions_carina.role_arn\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);
    let annotation_warning = diagnostics
        .iter()
        .find(|d| d.message.contains("type annotation required"));
    assert!(
        annotation_warning.is_none(),
        "module-call attribute export must not raise `type annotation required`. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined") || d.message.contains("unknown binding"));
    assert!(
        undefined.is_none(),
        "module-call binding must not be flagged as unknown. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn postfix_index_on_sibling_computed_let_has_no_unknown_binding_diagnostic() {
    let engine = test_engine();
    let directory = tempfile::tempdir().unwrap();
    let main = r#"
let queue = mock.queue.Queue {
  queue_name = "demo"
}
let parts = split(":", queue.arn)
"#;
    let exports = r#"
exports {
  account_id: String = parts[5]
}
"#;
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
    std::fs::write(directory.path().join("exports.crn"), exports).unwrap();

    let path = directory.path().join("exports.crn");
    let diagnostics = engine.analyze(&create_document_at(exports, path.clone()), Some(&path));
    let false_unknown = diagnostics.iter().find(|diagnostic| {
        diagnostic.message.contains("unknown binding 'parts[5]'")
            || diagnostic.message.contains("Undefined resource 'parts[5]'")
    });

    assert!(
        false_unknown.is_none(),
        "postfix access must preserve the sibling `parts` binding; diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn postfix_access_type_errors_match_validate_across_sibling_files() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let directory = tempfile::tempdir().unwrap();
    let values = r#"
let q = mock.test.resource { name = "q" }
let parts = split("-", q.identifier)
"#;
    let main = r#"
let a = mock.test.resource { name = parts.foo }
let b = mock.test.resource { name = parts[1][0] }
let r = mock.test.resource { name = split("-", q.identifier)[0][0] }
"#;
    std::fs::write(directory.path().join("values.crn"), values).unwrap();
    std::fs::write(directory.path().join("main.crn"), main).unwrap();

    let diagnostics = analyze_with_buffer(&engine, directory.path(), "main.crn", main);
    let access_errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("cannot apply access"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        access_errors.len(),
        3,
        "LSP must surface every Access error reported by validate: {diagnostics:#?}",
    );
    assert!(
        access_errors
            .iter()
            .any(|message| message.contains(".foo") && message.contains("list(String)")),
        "missing field-on-list diagnostic: {diagnostics:#?}",
    );
    assert_eq!(
        access_errors
            .iter()
            .filter(|message| message.contains("[0]") && message.contains("String"))
            .count(),
        2,
        "missing nested string-index diagnostics: {diagnostics:#?}",
    );
}

#[test]
fn secret_subscript_parse_diagnostic_is_redacted_in_lsp() {
    let directory = tempfile::tempdir().unwrap();
    let source = r#"
let parts = ["a", "b"]
let selected = parts[secret("hunter2")]
"#;
    std::fs::write(directory.path().join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&test_engine(), directory.path(), "main.crn", source);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("index access key must be an integer or string")),
        "missing invalid-subscript diagnostic: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("hunter2")),
        "secret subscript plaintext leaked through the LSP: {diagnostics:#?}",
    );
}

#[test]
fn module_call_export_typo_reports_attached_inference_failure_once_after_lsp_expansion() {
    let engine = DiagnosticEngine::new(
        loaded_test_providers(SchemaRegistry::new()),
        vec!["awscc".to_string()],
    );
    let tmp = tempfile::tempdir().unwrap();
    let module_dir = tmp.path().join("github_module");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  name: String\n}\nattributes {\n  role_arn: String = name\n}\n",
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let github = use { source = '../github_module' }\nlet github_actions_carina = github { name = 'demo' }\n",
    )
    .unwrap();
    let exports = "exports {\n  role_arn = github_actions_carina.role_ar\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);
    assert_eq!(
        diagnostics.len(),
        1,
        "post-expansion inference and the existence walk must not duplicate the typo: {diagnostics:#?}",
    );
    assert_eq!(
        diagnostics[0].message,
        "export 'role_arn': type annotation required: unknown attribute `role_ar` on `github_actions_carina`",
    );
}

#[test]
fn module_call_attribute_typo_in_resource_value_uses_expanded_binding_index() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let consumer_schema = ResourceSchema::new("test.Consumer")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target_group_arn", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        consumer_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let module_dir = tmp.path().join("component_module");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(
        module_dir.join("main.crn"),
        "arguments {\n  name: String\n}\nattributes {\n  role_arn: String = name\n}\n",
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let component = use { source = '../component_module' }\nlet instance = component { name = 'demo' }\n",
    )
    .unwrap();
    let resources = r#"let consumer = awscc.test.Consumer {
  name             = "caller"
  target_group_arn = instance.role_ar
}
"#;
    std::fs::write(base.join("resources.crn"), resources).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "resources.crn", resources);
    let typo_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("role_ar")
                && diagnostic
                    .message
                    .to_ascii_lowercase()
                    .contains("unknown attribute")
        })
        .collect();

    assert_eq!(
        typo_diagnostics.len(),
        1,
        "resource references must validate module-call attributes against the expanded composition: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        typo_diagnostics[0].message,
        "Unknown attribute 'role_ar' on 'instance'. Did you mean 'role_arn'?",
        "composition diagnostics should follow the LSP's own unknown-attribute wording",
    );
}

#[test]
fn schema_attribute_typo_in_module_call_argument_is_reported() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let target_group_schema = ResourceSchema::new("elbv2.TargetGroup")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target_group_arn", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        target_group_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let consumer_module = tmp.path().join("consumer_module");
    std::fs::create_dir_all(&consumer_module).unwrap();
    std::fs::write(
        consumer_module.join("main.crn"),
        "arguments {\n  arn: String\n}\nattributes {\n  arn = arn\n}\n",
    )
    .unwrap();

    let base = tmp.path().join("caller");
    std::fs::create_dir_all(&base).unwrap();
    let source = r#"let consumer = use { source = '../consumer_module' }

let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}

let b = consumer {
  arn = tg.target_group_ar
}
"#;
    std::fs::write(base.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", source);
    let typo_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("target_group_ar")
                && diagnostic.message.starts_with("module call 'b':")
                && diagnostic
                    .message
                    .to_ascii_lowercase()
                    .contains("unknown attribute")
        })
        .collect();

    assert_eq!(
        typo_diagnostics.len(),
        1,
        "the shared module-call validator should report the schema-backed typo once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert!(
        typo_diagnostics[0]
            .message
            .contains("module call 'b': unknown attribute 'target_group_ar' on 'tg'"),
        "the LSP diagnostic should come from the shared module-call validator: {:?}",
        typo_diagnostics[0].message,
    );
    assert_eq!(
        typo_diagnostics[0].range.start.line, 7,
        "the authored argument value should anchor the diagnostic",
    );
    assert_eq!(typo_diagnostics[0].range.start.character, 8);
    assert_eq!(typo_diagnostics[0].range.end.character, 26);
}

#[test]
fn module_call_argument_attribute_typo_is_reported_without_a_base_path() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let target_group_schema = ResourceSchema::new("elbv2.TargetGroup")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target_group_arn", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        target_group_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = r#"let consumer = use { source = '../consumer_module' }

let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}

let b = consumer {
  arn = tg.target_group_ar
}
"#;
    let doc = create_document(source);

    let diagnostics = engine.analyze(&doc, None);
    let typo_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("target_group_ar")
                && diagnostic
                    .message
                    .to_ascii_lowercase()
                    .contains("unknown attribute")
        })
        .collect();

    assert_eq!(
        typo_diagnostics.len(),
        1,
        "module-call argument refs must retain existence checking without filesystem context: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(typo_diagnostics[0].range.start.line, 7);
    assert_eq!(typo_diagnostics[0].range.start.character, 8);
    assert_eq!(typo_diagnostics[0].range.end.character, 26);
}

fn module_boundary_identity_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};

    let identity = |path: &str, kind: &str| {
        AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type("aws", path, kind)),
            None,
            None,
            None,
        )
    };
    let vpc = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", identity("ec2.Vpc", "Id")).build());
    let security_group = ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", identity("ec2.Vpc", "Id")).build())
        .attribute(
            AttributeSchema::optional("group_id", identity("ec2.SecurityGroup", "Id")).build(),
        );
    let log_group = ResourceSchema::new("logs.LogGroup")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("arn", identity("logs.LogGroup", "Arn")).build());
    let hosted_zone = ResourceSchema::new("route53.HostedZone").attribute(
        AttributeSchema::optional(
            "name",
            AttributeType::refined_string(None, None, Some((None, Some(1024))), None),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert("aws", vpc.validate().expect("schema fixture must be valid"));
    schemas.insert(
        "aws",
        security_group
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "aws",
        log_group.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "aws",
        hosted_zone
            .validate()
            .expect("schema fixture must be valid"),
    );
    custom_engine(schemas)
}

#[test]
fn module_argument_types_are_sources_in_open_module_document() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let source = r#"arguments {
  vpc_id: String
}

attributes {
  x: aws.ec2.Vpc.Id = vpc_id
}

let sg = aws.ec2.SecurityGroup {
  name   = "module"
  vpc_id = vpc_id
}
"#;
    std::fs::write(module.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &module, "main.crn", source);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("attribute 'x': type mismatch")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got String")
                && diagnostic.message.contains("from vpc_id")
        }),
        "the open module's attribute declaration must use its argument declaration as the source type: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got String")
                && diagnostic.message.contains("from vpc_id")
        }),
        "the open module's resource attribute must use its argument declaration as the source type: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn consumer_attribute_resolution_marks_unnameable_type_unchecked() {
    let parsed = carina_core::parser::parse_file(
        "attributes {\n  out: missing.types.Identifier = 'value'\n}\n",
        std::path::Path::new("module.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("open-module fixture must parse");
    let parameters = carina_core::validation::resolve_attribute_static_types_for_consumer(
        &parsed.attribute_params,
        &carina_core::parser::ProviderContext::default(),
    );

    assert!(matches!(
        &parameters[0].static_type,
        Some(carina_core::static_type::StaticType::Unchecked(reason))
            if reason.original()
                == &TypeExpr::DottedUnresolved(
                    carina_core::parser::ResourceTypePath::new(
                        "missing",
                        "types.Identifier",
                    ),
                )
    ));
}

#[test]
fn module_call_findings_render_parser_key_and_call_ranges() {
    let fixture = tempfile::tempdir().expect("module-call range fixture");
    let child = fixture.path().join("child");
    std::fs::create_dir(&child).expect("child module directory");
    std::fs::write(child.join("arguments.crn"), "arguments { count: Int }\n")
        .expect("child signature");
    let source = r#"let child = use { source = "./child" }
child {
  count = 1
  surprise = true
}
child {}
"#;
    let path = fixture.path().join("calls.crn");
    std::fs::write(&path, source).expect("root calls");
    let doc = create_document_at(source, path.clone());
    let parsed = doc.parsed().expect("fixture must parse");
    let diagnostics = test_engine().analyze(&doc, Some(&path));

    let unknown = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Unknown argument 'surprise'"))
        .unwrap_or_else(|| panic!("missing unknown-argument diagnostic: {diagnostics:#?}"));
    let unknown_span = parsed.module_calls[0]
        .arguments_with_source_spans()
        .find(|argument| argument.name() == "surprise")
        .expect("surprise argument")
        .key_span();
    assert_eq!(
        unknown.range,
        parser_span_range(&doc, unknown_span).unwrap()
    );
    assert_eq!(unknown.severity, Some(DiagnosticSeverity::ERROR));

    let missing = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("Missing required argument 'count'")
        })
        .unwrap_or_else(|| panic!("missing required-argument diagnostic: {diagnostics:#?}"));
    assert_eq!(
        missing.range,
        parser_span_range(&doc, parsed.module_calls[1].source_span()).unwrap(),
    );
    assert_eq!(missing.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn nested_module_call_finding_renders_on_the_root_call_range() {
    let fixture = tempfile::tempdir().expect("nested module-call range fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    std::fs::create_dir(&outer).expect("outer module directory");
    std::fs::create_dir(&inner).expect("inner module directory");
    std::fs::write(outer.join("arguments.crn"), "arguments { value: String }\n")
        .expect("outer signature");
    std::fs::write(
        outer.join("imports.crn"),
        "let inner = use { source = \"../inner\" }\n",
    )
    .expect("inner import");
    std::fs::write(outer.join("calls.crn"), "inner { value = value }\n").expect("nested call");
    std::fs::write(inner.join("arguments.crn"), "arguments { value: Int }\n")
        .expect("inner signature");
    let source = r#"let outer = use { source = "./outer" }
outer { value = "root" }
"#;
    let path = fixture.path().join("calls.crn");
    std::fs::write(&path, source).expect("root call");
    let doc = create_document_at(source, path.clone());
    let parsed = doc.parsed().expect("fixture must parse");
    let diagnostics = test_engine().analyze(&doc, Some(&path));

    let nested = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'inner (anonymous call)'")
        })
        .unwrap_or_else(|| panic!("missing nested module-call diagnostic: {diagnostics:#?}"));
    assert_eq!(
        nested.range,
        parser_span_range(&doc, parsed.module_calls[0].source_span()).unwrap(),
    );
    assert_eq!(nested.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn module_argument_postfix_access_uses_declared_type_in_open_module_document() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("module");
    std::fs::create_dir(&module).unwrap();
    let source = r#"arguments {
  items: list(String)
}

let sg = aws.ec2.SecurityGroup {
  name   = "module"
  vpc_id = items[0]
}
"#;
    std::fs::write(module.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &module, "main.crn", source);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got String")
                && diagnostic.message.contains("module argument: items")
        }),
        "postfix access must use the module argument declaration as inference authority: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn composition_diagnostic_does_not_fall_back_to_unrelated_first_call() {
    use carina_core::binding_index::BindingIndex;
    use carina_core::parser::{ArgumentOrigin, TypeExpr};
    use carina_core::resource::{
        CompositionCall, CompositionCallPath, ConcreteValue, LiveComposition,
        LiveCompositionArgument, LiveCompositionAttribute, LiveCompositionSignature,
        ParserPlaceholder, ResourceId, Value,
    };
    use indexmap::IndexMap;

    fn schema_type(path: &str) -> TypeExpr {
        TypeExpr::SchemaType {
            owned: carina_core::parser::OwnedTypeIdentity::provider(
                "aws",
                carina_core::schema::TypeIdentity::from_schema_type("aws", path, "Id"),
            ),
        }
    }

    fn consumer_type(type_expr: TypeExpr) -> carina_core::static_type::ConsumerStaticType {
        carina_core::validation::resolve_consumer_static_type_for_signature(
            &type_expr,
            &carina_core::parser::ProviderContext::default(),
        )
    }

    fn composition(
        binding: &'static str,
        call_binding: &str,
    ) -> LiveComposition<ParserPlaceholder> {
        let source_file = std::path::PathBuf::from(format!("/{call_binding}.crn"));
        let parsed = carina_core::parser::parse_file(
            "let missing = use { source = '../missing' }\nmissing {}\n",
            &source_file,
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("parse synthetic composition call");
        let call = CompositionCall {
            id: parsed.module_calls[0].id().clone(),
            module_name: "missing".to_string(),
            binding: Some(call_binding.to_string()),
            instance: call_binding.to_string(),
            module_source: Some("../missing".to_string()),
            module_directory: None,
            loop_origin: None,
        };
        LiveComposition {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", binding,
            )),
            signature: LiveCompositionSignature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
                pending_constraints: Vec::new(),
            },
            binding: Some(binding.to_string()),
            dependency_bindings: Default::default(),
            module_name: "missing".to_string(),
            instance: call_binding.to_string(),
            call_path: CompositionCallPath::direct(call),
            quoted_string_attrs: Default::default(),
        }
    }

    let engine = module_boundary_identity_engine();
    let doc = create_document(
        r#"let unrelated = use { source = '../unrelated' }
let first = unrelated { }
"#,
    );
    let parsed = doc.parsed().expect("fixture must parse");
    let mut binding_input = parsed.clone();

    let mut source = composition("source", "missing-source-call");
    source.signature.attributes.insert(
        "sg".to_string(),
        LiveCompositionAttribute::from_value(
            Value::Concrete(ConcreteValue::String("sg-123".into())),
            Some(consumer_type(schema_type("ec2.SecurityGroup"))),
        ),
    );
    let mut consumer = composition("consumer", "missing-consumer-call");
    consumer.signature.arguments.insert(
        "vpc_id".to_string(),
        LiveCompositionArgument::from_value(
            Value::resource_ref("source", "missing", vec![]),
            consumer_type(schema_type("ec2.Vpc")),
            ArgumentOrigin::Defaulted,
        ),
    );
    binding_input.compositions.extend([source, consumer]);
    let bindings = BindingIndex::from_parsed(&binding_input, &engine.schemas);
    let findings = carina_core::validation::check_expanded_module_calls(
        carina_core::validation::ExpandedModuleCallCheckInput::References {
            compositions: &binding_input.compositions,
            bindings: &bindings,
        },
    );
    let diagnostics = render_module_call_findings(&doc, doc.source_file(), &findings);

    assert!(
        diagnostics.is_empty(),
        "an error whose immediate and root calls are absent from this document must not attach to its first unrelated call: {diagnostics:?}",
    );
}

#[test]
fn module_attribute_schema_type_declaration_mismatch_matches_validate() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("resources.crn"),
        r#"let vpc = aws.ec2.Vpc {
  name = "main"
}

let sg = aws.ec2.SecurityGroup {
  name   = "sg"
  vpc_id = vpc.vpc_id
}
"#,
    )
    .unwrap();
    let attributes = r#"attributes {
  bad: aws.ec2.Vpc.Id = sg.group_id
}
"#;
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);

    let mismatch = diagnostics.iter().find(|diagnostic| {
        diagnostic
            .message
            .contains("attribute 'bad': type mismatch")
            && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
            && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
            && diagnostic.message.contains("from sg.group_id")
    });
    assert!(
        mismatch.is_some(),
        "LSP must report the same declaration mismatch as validate: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    let mismatch = mismatch.unwrap();
    assert_eq!(mismatch.range.start.line, 1);
    assert_eq!(mismatch.range.start.character, 2);
}

#[test]
fn declared_string_output_preserves_forwarded_evidence_in_lsp() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("module");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"attributes {
  x: String = lg.arn
}

let lg = aws.logs.LogGroup {
  name = "module"
}
"#,
    )
    .unwrap();
    let source = r#"let component = use { source = '../module' }
let m = component { }

let zone = aws.route53.HostedZone {
  name = m.x
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    let mismatch = diagnostics.iter().find(|diagnostic| {
        diagnostic.message.contains("Type mismatch")
            && diagnostic.message.contains("expected String(len: ..=1024)")
            && diagnostic.message.contains("got aws.logs.LogGroup.Arn")
            && diagnostic.message.contains("from m.x, declared String")
    });
    assert!(
        mismatch.is_some(),
        "LSP must retain the same inferred evidence as validate: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn expanded_module_attribute_declaration_error_stays_in_module_document() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("bad_module");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let module_source = r#"arguments {
  name: String
}

attributes {
  bad: aws.ec2.Vpc.Id = sg.group_id
}

let sg = aws.ec2.SecurityGroup {
  name = name
}
"#;
    std::fs::write(module.join("main.crn"), module_source).unwrap();
    let root_source = r#"let bad_module = use { source = '../bad_module' }

let one = bad_module {
  name = "one"
}
"#;
    let sibling_source = r#"let unrelated = aws.ec2.SecurityGroup {
  name = "unrelated"
}
"#;
    std::fs::write(root.join("main.crn"), root_source).unwrap();
    std::fs::write(root.join("other.crn"), sibling_source).unwrap();

    let root_diagnostics = analyze_with_buffer(&engine, &root, "main.crn", root_source);
    let sibling_diagnostics = analyze_with_buffer(&engine, &root, "other.crn", sibling_source);
    for diagnostics in [&root_diagnostics, &sibling_diagnostics] {
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("attribute 'bad'")),
            "an imported module declaration belongs to the module document: {:?}",
            diagnostics
                .iter()
                .map(|diagnostic| (&diagnostic.range, &diagnostic.message))
                .collect::<Vec<_>>(),
        );
    }

    let module_diagnostics = analyze_with_buffer(&engine, &module, "main.crn", module_source);
    let declaration_diagnostics: Vec<_> = module_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("attribute 'bad'"))
        .collect();
    assert_eq!(declaration_diagnostics.len(), 1);
    assert_eq!(declaration_diagnostics[0].range.start.line, 5);
    assert_eq!(declaration_diagnostics[0].range.start.character, 2);
}

#[test]
fn expanded_call_boundary_diagnostic_is_owned_by_calling_sibling() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("needs_vpc");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .unwrap();
    let main_source = r#"let unrelated = aws.ec2.SecurityGroup {
  name = "unrelated"
}
"#;
    let calls_source = r#"let needs_vpc = use { source = '../needs_vpc' }

let sg = aws.ec2.SecurityGroup {
  name = "source"
}

let broken = needs_vpc {
  vpc_id = sg.group_id
}
"#;
    std::fs::write(root.join("main.crn"), main_source).unwrap();
    std::fs::write(root.join("calls.crn"), calls_source).unwrap();

    let main_diagnostics = analyze_with_buffer(&engine, &root, "main.crn", main_source);
    assert!(
        main_diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("module call 'broken'")),
        "a sibling call diagnostic must not leak into the open document: {:?}",
        main_diagnostics
            .iter()
            .map(|diagnostic| (&diagnostic.range, &diagnostic.message))
            .collect::<Vec<_>>(),
    );

    let call_diagnostics = analyze_with_buffer(&engine, &root, "calls.crn", calls_source);
    let mismatches: Vec<_> = call_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("module call 'broken'"))
        .collect();
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].range.start.line, 7);
    assert_eq!(mismatches[0].range.start.character, 11);
    assert_eq!(mismatches[0].range.end.character, 22);
}

#[test]
fn reference_valued_module_constraint_stays_pending_without_internal_diagnostic() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("checked");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}
"#,
    )
    .unwrap();
    let source = r#"let checked = use { source = '../checked' }

let producer = aws.logs.LogGroup {
  name = "producer"
}

let instance = checked {
  name = producer.arn
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    for forbidden in [
        "name must not be empty",
        "Deferred(",
        "ResourceRef {",
        "length()",
    ] {
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains(forbidden)),
            "pending reference-valued validation leaked {forbidden:?}: {diagnostics:#?}",
        );
    }
}

#[test]
fn concrete_invalid_module_argument_reports_authored_validation_at_argument() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("checked");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}
"#,
    )
    .unwrap();
    let source = r#"let checked = use { source = '../checked' }

let instance = checked {
  name = ""
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("name must not be empty"))
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "authored validation must be reported exactly once: {diagnostics:#?}"
    );
    let diagnostic = matching[0];
    assert_eq!(diagnostic.range.start.line, 3);
    assert_eq!(diagnostic.range.start.character, 9);
    assert_eq!(diagnostic.range.end.character, 11);
}

#[test]
fn concrete_invalid_module_require_reports_authored_message_at_call() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("checked");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  left: String
  right: String
}

require left == right, "left and right must match"
"#,
    )
    .unwrap();
    let source = r#"let checked = use { source = '../checked' }

let instance = checked {
  left  = "a"
  right = "b"
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("left and right must match"))
        .collect::<Vec<_>>();
    assert_eq!(
        matching.len(),
        1,
        "authored require must be reported exactly once: {diagnostics:#?}"
    );
    let diagnostic = matching[0];
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.start.character, 15);
}

#[test]
fn concrete_module_argument_postfix_miss_is_reported_by_lsp() {
    let engine = test_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("modules/selector");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { items: list(String) }\n",
    )
    .unwrap();
    std::fs::write(
        module.join("resource.crn"),
        "\nlet selected = mock.test.Thing {\n  name = items[5]\n}\n",
    )
    .unwrap();
    let source = r#"let selector = use { source = "../modules/selector" }

let child = selector { items = ["x", "y"] }
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("index 5 out of range for list of length 2")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        matching[0].message.contains("selector")
            && matching[0].message.contains("modules/selector")
            && matching[0].message.contains("line 3"),
        "diagnostic must retain module source context: {:#?}",
        matching[0]
    );
}

#[test]
fn bare_structural_handle_is_a_real_engine_validation_error() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.network")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = r#"
let vpc = mock.test.network {
  name = "network"
}

mock.test.resource {
  name = "child"
  parent = vpc
}
"#;

    let document = create_document(source);
    assert!(
        document.parsed().is_some(),
        "the parser must preserve the authored use for typed validation"
    );
    let diagnostics = engine.analyze(&document, None);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .unwrap_or_else(|| panic!("missing bare-handle diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostic.range.start.line, 7);
}

#[test]
fn sibling_bare_structural_handle_is_a_real_engine_parse_error() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.network")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let tmp = tempfile::tempdir().unwrap();
    let source = r#"mock.test.resource {
  name = "child"
  parent = vpc
}
"#;
    std::fs::write(tmp.path().join("main.crn"), source).unwrap();
    std::fs::write(
        tmp.path().join("network.crn"),
        r#"let vpc = mock.test.network {
  name = "network"
}
"#,
    )
    .unwrap();

    let path = tmp.path().join("main.crn");
    let document = create_document_at(source, path.clone());
    assert!(
        document.parsed().is_some(),
        "the buffer-only parser cannot yet classify a sibling name"
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .unwrap_or_else(|| panic!("missing sibling bare-handle diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostic.range.start.line, 2);
}

#[test]
fn module_argument_substitution_error_is_anchored_to_the_authored_argument() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("identifier", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.Consumer")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let module = tmp.path().join("consumer");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments { peer: mock.test.Thing }
let consumer = mock.test.Consumer {
  name = "consumer"
  directives { depends_on = [peer] }
}
"#,
    )
    .unwrap();
    let source = r#"
let source = mock.test.Thing { name = "peer" }
let imported = use { source = "../consumer" }
let child = imported {
  peer =
    source.identifier
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let path = root.join("main.crn");
    let document = create_document_at(source, path.clone());
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("depends_on parameter 'peer' received a value")
        })
        .unwrap_or_else(|| panic!("missing substitution diagnostic: {diagnostics:#?}"));

    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostic.range.start.line, 5, "{diagnostic:#?}");
    assert_eq!(diagnostic.range.end.line, 5, "{diagnostic:#?}");
}

#[test]
fn resolver_error_with_repeated_module_name_anchors_the_failing_call() {
    let main = "let selector = use { source = './selector' }\n\
let a = selector { items = ['a', 'b', 'c', 'd', 'e', 'f'] }\n\
let b = selector { items = ['x', 'y'] }\n";
    let fixture = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        (
            "selector/arguments.crn",
            "arguments { items: list(String) }\n",
        ),
        (
            "selector/resource.crn",
            "mock.test.resource { name = items[5] }\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), fixture.path(), "main.crn", main);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("index 5 out of range for list of length 2")
        })
        .unwrap_or_else(|| panic!("missing resolver access diagnostic: {diagnostics:#?}"));

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(2, 8), Position::new(2, 16)),
        "the second call owns the failing argument value"
    );
}

#[test]
fn imported_deferred_body_rejection_is_anchored_in_the_caller() {
    let caller = "let parent = use { source = './parent' }\n\
let instance = parent { names = ['one'] }\n";
    let fixture = write_tag_key_fixture(&[
        ("caller.crn", caller),
        (
            "parent/main.crn",
            "arguments { names: list(String) }\n\
             let grandchild = use { source = './grandchild' }\n\
             let children = for name in names { grandchild { name = name } }\n",
        ),
        ("parent/grandchild/main.crn", "arguments { name: String }\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), fixture.path(), "caller.crn", caller);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("cannot use module call `grandchild`")
        })
        .unwrap_or_else(|| panic!("missing deferred-body diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.range.start.line, 1, "{diagnostic:#?}");
    assert_eq!(diagnostic.range.start.character, 15, "{diagnostic:#?}");
}

#[test]
fn imported_syntax_error_remains_anchored_at_the_owning_call() {
    let caller = "let broken = use { source = './broken' }\n\
broken {}\n";
    let fixture = write_tag_key_fixture(&[
        ("caller.crn", caller),
        ("broken/main.crn", "arguments { value: String\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), fixture.path(), "caller.crn", caller);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Syntax error"))
        .unwrap_or_else(|| panic!("missing imported syntax diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.range.start.line, 1, "{diagnostic:#?}");
    assert_eq!(diagnostic.range.start.character, 0, "{diagnostic:#?}");
    assert_eq!(diagnostic.range.end.character, 6, "{diagnostic:#?}");
    assert!(diagnostic.message.contains("broken/main.crn"));
    assert!(!diagnostic.message.contains("module call in caller.crn"));
}

#[test]
fn heredoc_argument_type_diagnostic_anchors_the_argument_value() {
    let main = r#"let m = use { source = './m' }
let inst = m {
  enabled = <<EOT
yes
EOT
}
"#;
    let fixture = write_tag_key_fixture(&[
        ("main.crn", main),
        ("m/arguments.crn", "arguments { enabled: Bool }\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), fixture.path(), "main.crn", main);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.message.contains("expected Bool")
                && diagnostic.message.contains("got string")
        })
        .unwrap_or_else(|| panic!("missing heredoc argument type diagnostic: {diagnostics:#?}"));

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(2, 12), Position::new(4, 3)),
        "type mismatches must underline the authored argument value"
    );
}

fn nested_bare_handle_fixture() -> (DiagnosticEngine, tempfile::TempDir, String) {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.network")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let main = "let a = use { source = './a' }\na {}\n".to_string();
    let fixture = write_tag_key_fixture(&[
        ("main.crn", &main),
        ("a/import.crn", "let b = use { source = '../b' }\n"),
        (
            "a/body.crn",
            "let vpc = mock.test.network { name = 'network' }\nb { parent = vpc }\nattributes { ok = 'ok' }\n",
        ),
        ("b/arguments.crn", "arguments { parent: String }\n"),
    ]);
    (engine, fixture, main)
}

#[test]
fn nested_bare_handle_diagnostic_is_anchored_at_the_root_call() {
    let (engine, fixture, main) = nested_bare_handle_fixture();

    let diagnostics = analyze_with_buffer(&engine, fixture.path(), "main.crn", &main);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .unwrap_or_else(|| panic!("missing nested bare-handle diagnostic: {diagnostics:#?}"));

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(1, 0), Position::new(1, 4)),
        "the root call must own a failure raised inside its imported module"
    );
}

#[test]
fn resolver_diagnostic_uses_explicit_current_file_path() {
    let (engine, fixture, main) = nested_bare_handle_fixture();
    let current_file = fixture.path().join("main.crn");
    let document = create_document_at(&main, current_file.clone());

    let diagnostics = engine.analyze(&document, Some(&current_file));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .unwrap_or_else(|| panic!("missing file-backed resolver diagnostic: {diagnostics:#?}"));

    assert_eq!(
        diagnostic.range,
        Range::new(Position::new(1, 0), Position::new(1, 4))
    );
}

#[test]
fn explicit_current_file_does_not_anchor_sibling_with_identical_span() {
    let engine = test_engine();
    let fixture = write_tag_key_fixture(&[
        ("main.crn", "let s = use { source = './safe' }\ns {}\n"),
        ("other.crn", "let a = use { source = './aaaa' }\na {}\n"),
        ("safe/attributes.crn", "attributes { ok = 'ok' }\n"),
        ("aaaa/import.crn", "let b = use { source = '../bbbb' }\n"),
        (
            "aaaa/body.crn",
            "let vpc = mock.test.network { name = 'network' }\nb { parent = vpc }\nattributes { ok = 'ok' }\n",
        ),
        ("bbbb/arguments.crn", "arguments { parent: String }\n"),
    ]);
    let current_source = "let s = use { source = './safe' }\ns {}\n";
    let path = fixture.path().join("main.crn");
    let document = create_document_at(current_source, path.clone());

    let diagnostics = engine.analyze(&document, Some(&path));

    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("'vpc' is a resource, not a value")),
        "a sibling failure must not be anchored on a different current file: \
         {diagnostics:#?}"
    );
}

#[test]
fn resolver_module_call_error_without_covering_finding_falls_back_to_root_call() {
    let engine = test_engine();
    let fixture = tempfile::tempdir().expect("resolver ownership fixture");
    let source = "let missing = use { source = './missing' }\nmissing {}\n";
    std::fs::write(fixture.path().join("main.crn"), source).expect("root source");

    let path = fixture.path().join("main.crn");
    let document = create_document_at(source, path.clone());
    let resolver_input = document.parsed().expect("root parse").clone();
    let report = match &engine.provider_context {
        DiagnosticProviderContext::Loaded(provider_context) => {
            carina_core::module_resolver::resolve_modules_with_diagnostics(
                &resolver_input,
                fixture.path(),
                provider_context,
                &engine.schemas,
                &[],
            )
        }
        DiagnosticProviderContext::Unloaded(provider_context) => {
            carina_core::module_resolver::resolve_modules_with_providerless_diagnostics(
                &resolver_input,
                fixture.path(),
                provider_context,
                &engine.schemas,
                &[],
            )
        }
    };
    let residuals = match report {
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::Expanded { graph } => {
            panic!(
                "missing module unexpectedly expanded to {} compositions",
                graph.compositions.len()
            )
        }
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::ConstraintFailed {
            findings,
            errors,
        } => panic!(
            "unexpected constraint outcome: {} findings and {} residuals",
            findings.len(),
            errors.len()
        ),
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::Unowned { errors } => {
            errors
        }
    };
    let residual = residuals
        .first()
        .expect("resolver fixture must retain the uncovered nested call error");
    assert!(
        matches!(
            residual.diagnostic(),
            Some(carina_core::module_resolver::ResidualModuleResolutionDiagnostic::ModuleCall)
        ),
        "unexpected residual: {}",
        residual.display_relative_to(fixture.path()),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let fallback = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("IO error: No such file or directory")
        })
        .unwrap_or_else(|| panic!("missing resolver fallback: {diagnostics:#?}"));

    assert_eq!(fallback.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(fallback.range.start.line, 1);
    assert_eq!(fallback.range.start.character, 0);
}

#[test]
fn bare_structural_handle_for_string_module_argument_is_an_error() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.network")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let module = tmp.path().join("module");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { parent: String }\n",
    )
    .unwrap();
    let source = r#"let vpc = mock.test.network { name = "network" }
let imported = use { source = "../module" }
let instance = imported { parent = vpc }
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let path = root.join("main.crn");
    let document = create_document_at(source, path.clone());
    assert!(
        document.parsed().is_some(),
        "module arguments retain handles"
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .unwrap_or_else(|| panic!("missing module-argument diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostic.range.start.line, 2);
}

fn multifile_sibling_let_module_constraint_diagnostics(
    name: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("checked");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) <= 3
      error_message = "name must be at most three characters"
    }
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("providers.crn"), "provider aws {}\n").unwrap();
    std::fs::write(
        tmp.path().join("consts.crn"),
        format!("let n = \"{name}\"\n"),
    )
    .unwrap();
    let main = r#"let checked = use { source = './checked' }

checked {
  name = n
}
"#;
    std::fs::write(tmp.path().join("main.crn"), main).unwrap();

    analyze_with_buffer(&engine, tmp.path(), "main.crn", main)
}

#[test]
fn multifile_sibling_let_satisfying_module_constraint_has_no_lsp_diagnostic() {
    let diagnostics = multifile_sibling_let_module_constraint_diagnostics("ab");

    assert!(
        diagnostics.is_empty(),
        "the LSP must evaluate the expanded sibling let value: {diagnostics:#?}"
    );
}

#[test]
fn multifile_sibling_let_violating_module_constraint_has_one_lsp_diagnostic() {
    let diagnostics = multifile_sibling_let_module_constraint_diagnostics("abcd");
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("name must be at most three characters")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "the expanded sibling let violation must be reported once: {diagnostics:#?}"
    );
    assert!(
        matching[0].message.contains("'abcd'") && !matching[0].message.contains("enum identifier"),
        "the diagnostic must render the expanded value: {diagnostics:#?}"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(3, 9), Position::new(3, 10)),
        "an authored argument constraint must underline its value"
    );
}

fn multifile_for_before_checked_constraint_diagnostics(
    name: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let checked = tmp.path().join("checked");
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&checked).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) <= 3
      error_message = "name must be at most three characters"
    }
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .unwrap();
    std::fs::write(
        other.join("arguments.crn"),
        "arguments {\n  label: String\n}\n",
    )
    .unwrap();
    std::fs::write(
        other.join("attributes.crn"),
        "attributes {\n  label = label\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("consts.crn"),
        format!("let labels = [\"one\", \"two\"]\nlet n = \"{name}\"\n"),
    )
    .unwrap();
    std::fs::write(tmp.path().join("providers.crn"), "provider aws {}\n").unwrap();
    let main = r#"let other = use { source = './other' }
let checked = use { source = './checked' }

let many = for x in labels { other { label = x } }

checked {
  name = n
}
"#;
    std::fs::write(tmp.path().join("main.crn"), main).unwrap();

    analyze_with_buffer(&engine, tmp.path(), "main.crn", main)
}

#[test]
fn multifile_expanded_for_does_not_shift_checked_constraint_diagnostic() {
    let diagnostics = multifile_for_before_checked_constraint_diagnostics("abcd");
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("name must be at most three characters")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "the resolver-owned checked-call failure must be emitted once: {diagnostics:#?}"
    );
    assert_eq!(
        matching[0].message,
        "anonymous call to module 'checked': argument 'name': name must be at most three characters (got 'abcd')"
    );
    assert_eq!(matching[0].range.start.line, 6);
}

#[test]
fn multifile_expanded_for_before_valid_checked_call_has_no_constraint_diagnostic() {
    let diagnostics = multifile_for_before_checked_constraint_diagnostics("ab");

    assert!(
        diagnostics.is_empty(),
        "a valid checked call must not inherit another expanded call's values: {diagnostics:#?}"
    );
}

#[test]
fn multifile_forward_reference_default_uses_resolver_constraint_outcome() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let checked = tmp.path().join("checked");
    std::fs::create_dir_all(&checked).unwrap();
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  prefix: String {
    default = later
    validation {
      condition     = length(prefix) <= 3
      error_message = "prefix must be at most three characters"
    }
  }
  later: String = "abcdef"
}
"#,
    )
    .unwrap();
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  prefix = prefix\n}\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("consts.crn"), "let unrelated = true\n").unwrap();
    std::fs::write(tmp.path().join("providers.crn"), "provider aws {}\n").unwrap();
    let main = r#"let checked = use { source = './checked' }

checked {}
"#;
    std::fs::write(tmp.path().join("main.crn"), main).unwrap();

    let diagnostics = analyze_with_buffer(&engine, tmp.path(), "main.crn", main);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("prefix must be at most three characters")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        matching.len(),
        1,
        "the LSP must consume the expander's fixed-point result: {diagnostics:#?}"
    );
    assert_eq!(
        matching[0].message,
        "anonymous call to module 'checked': argument 'prefix': prefix must be at most three characters (got 'abcdef')"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(2, 0), Position::new(2, 10)),
        "a defaulted argument constraint must underline its owning call"
    );
}

#[test]
fn nested_concrete_module_constraint_survives_merged_expansion_failure() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let checked = tmp.path().join("checked");
    let outer = tmp.path().join("outer");
    let root = tmp.path().join("root");
    for directory in [&checked, &outer, &root] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        checked.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = "nested name must not be empty"
    }
  }
}
"#,
    )
    .unwrap();
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments {
  enabled: Bool = true
}

let checked = use { source = '../checked' }

let nested = checked {
  name = ""
}
"#,
    )
    .unwrap();
    let source = r#"let outer = use { source = '../outer' }

let instance = outer {
}
"#;
    std::fs::write(root.join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", source);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("nested name must not be empty"))
        .unwrap_or_else(|| panic!("nested resolver failure was lost: {diagnostics:#?}"));
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.start.character, 15);
}

#[test]
fn composition_consumer_identity_mismatch_matches_validate() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("module");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .unwrap();
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  security_group_id = sg.group_id\n}\n",
    )
    .unwrap();
    std::fs::write(
        module.join("resources.crn"),
        r#"let sg = aws.ec2.SecurityGroup {
  name   = "module"
  vpc_id = vpc_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("main.crn"),
        r#"let component = use { source = '../module' }

let vpc = aws.ec2.Vpc {
  name = "main"
}

let instance = component {
  vpc_id = vpc.vpc_id
}
"#,
    )
    .unwrap();
    let resources = r#"let consumer = aws.ec2.SecurityGroup {
  name   = "consumer"
  vpc_id = instance.security_group_id
}
"#;
    std::fs::write(root.join("resources.crn"), resources).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "resources.crn", resources);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic
                    .message
                    .contains("from instance.security_group_id")
        }),
        "LSP must report the same composition-consumer mismatch as validate: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn composition_argument_identity_mismatch_matches_validate_at_call_boundary() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("module");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("resources.crn"),
        r#"let sg = aws.ec2.SecurityGroup {
  name   = "module-sg"
  vpc_id = vpc_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("attributes.crn"),
        r#"attributes {
  security_group_id = sg.group_id
}
"#,
    )
    .unwrap();
    let main = r#"let issue = use { source = '../module' }

let vpc = aws.ec2.Vpc {
  name = "root"
}

let web = issue {
  vpc_id = vpc.vpc_id
}

let web2 = issue {
  vpc_id = web.security_group_id
}
"#;
    std::fs::write(root.join("main.crn"), main).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", main);

    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'web2': argument 'vpc_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.message.contains("from web.security_group_id")
        })
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "LSP must report the root module-call boundary mismatch exactly once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    let mismatch = mismatches[0];
    assert_eq!(
        mismatch.range.start.line, 11,
        "the structured call label must anchor the second call, not the first",
    );
    assert_eq!(mismatch.range.start.character, 11);
    assert_eq!(mismatch.range.end.character, 32);
}

#[test]
fn identical_anonymous_module_calls_anchor_mismatches_at_their_own_spans() {
    let engine = module_boundary_identity_engine();
    let main = r#"let checked = use { source = './module' }

let sg = aws.ec2.SecurityGroup {
  name = "root"
}

checked {
  vpc_id = sg.group_id
}

checked {
  vpc_id = sg.group_id
}
"#;
    let fixture = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "module/main.crn",
            "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&engine, fixture.path(), "main.crn", main);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'checked (anonymous call)': argument 'vpc_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
        })
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        mismatches
            .iter()
            .map(|diagnostic| diagnostic.range)
            .collect::<Vec<_>>(),
        vec![
            Range::new(Position::new(7, 11), Position::new(7, 22)),
            Range::new(Position::new(11, 11), Position::new(11, 22)),
        ],
    );
}

#[test]
fn defaulted_composition_argument_mismatch_anchors_the_authored_call() {
    let engine = module_boundary_identity_engine();
    let main = r#"let checked = use { source = './module' }

let sg = aws.ec2.SecurityGroup {
  name = "root"
}

let instance = checked {
  source_id = sg.group_id
}
"#;
    let fixture = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "module/arguments.crn",
            r#"arguments {
  source_id: aws.ec2.SecurityGroup.Id
  target_id: aws.ec2.Vpc.Id = source_id
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&engine, fixture.path(), "main.crn", main);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'instance': argument 'target_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.message.contains("from sg.group_id")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(6, 15), Position::new(8, 1)),
        "a defaulted argument has no authored key, so the call must own the diagnostic",
    );
}

#[test]
fn nested_module_call_identity_mismatch_is_reported_when_outer_module_is_opened() {
    let engine = module_boundary_identity_engine();
    let tmp = tempfile::tempdir().unwrap();
    let web_tier = tmp.path().join("web_tier");
    let needs_vpc = tmp.path().join("needs_vpc");
    let outer = tmp.path().join("outer");
    for directory in [&web_tier, &needs_vpc, &outer] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        web_tier.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_sg = aws.ec2.SecurityGroup {
  name   = "web"
  vpc_id = vpc_id
}

attributes {
  sg_id: aws.ec2.SecurityGroup.Id = web_sg.group_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        needs_vpc.join("main.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .unwrap();
    let outer_source = r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_tier = use { source = '../web_tier' }
let needs_vpc = use { source = '../needs_vpc' }

let a = web_tier {
  vpc_id = vpc_id
}

let b = needs_vpc {
  vpc_id = a.sg_id
}
"#;
    std::fs::write(outer.join("main.crn"), outer_source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &outer, "main.crn", outer_source);
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'b': argument 'vpc_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.message.contains("from a.sg_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "the open outer module must report its nested call boundary once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(mismatches[0].range.start.line, 12);
    assert_eq!(mismatches[0].range.start.character, 11);
    assert_eq!(mismatches[0].range.end.character, 18);
}

struct NestedModuleCallLspFixture {
    _temp: tempfile::TempDir,
    outer: std::path::PathBuf,
    root: std::path::PathBuf,
    outer_source: String,
    root_source: String,
}

fn nested_module_call_lsp_fixture(attribute_declaration: &str) -> NestedModuleCallLspFixture {
    let temp = tempfile::tempdir().unwrap();
    let web_tier = temp.path().join("web_tier");
    let needs_vpc = temp.path().join("needs_vpc");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    for directory in [&web_tier, &needs_vpc, &outer, &root] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        web_tier.join("main.crn"),
        format!(
            r#"arguments {{
  vpc_id: aws.ec2.Vpc.Id
}}

let web_sg = aws.ec2.SecurityGroup {{
  name   = "web"
  vpc_id = vpc_id
}}

attributes {{
  sg_id{attribute_declaration} = web_sg.group_id
}}
"#
        ),
    )
    .unwrap();
    std::fs::write(
        needs_vpc.join("main.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .unwrap();
    let outer_source = r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_tier = use { source = '../web_tier' }
let needs_vpc = use { source = '../needs_vpc' }

let a = web_tier {
  vpc_id = vpc_id
}

let b = needs_vpc {
  vpc_id = a.sg_id
}
"#
    .to_string();
    std::fs::write(outer.join("main.crn"), &outer_source).unwrap();
    let root_source = r#"let outer = use { source = '../outer' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let instance = outer {
  vpc_id = main_vpc.vpc_id
}
"#
    .to_string();
    std::fs::write(root.join("main.crn"), &root_source).unwrap();

    NestedModuleCallLspFixture {
        _temp: temp,
        outer,
        root,
        outer_source,
        root_source,
    }
}

fn assert_one_nested_lsp_mismatch(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
    expected_call: &str,
    expected_path: &str,
) {
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains(&format!("module call '{expected_call}': argument 'vpc_id'"))
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic
                    .message
                    .contains(&format!("from {expected_path}"))
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "the LSP must report the nested call boundary once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn unannotated_nested_module_call_is_inferred_when_outer_module_is_opened() {
    let engine = module_boundary_identity_engine();
    let fixture = nested_module_call_lsp_fixture("");

    let diagnostics =
        analyze_with_buffer(&engine, &fixture.outer, "main.crn", &fixture.outer_source);

    assert_one_nested_lsp_mismatch(&diagnostics, "b", "a.sg_id");
}

#[test]
fn annotated_nested_module_call_is_reported_when_root_module_is_opened() {
    let engine = module_boundary_identity_engine();
    let fixture = nested_module_call_lsp_fixture(": aws.ec2.SecurityGroup.Id");

    let diagnostics = analyze_with_buffer(&engine, &fixture.root, "main.crn", &fixture.root_source);

    assert_one_nested_lsp_mismatch(&diagnostics, "b", "a.sg_id");
    let mismatch = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("module call 'b'"))
        .unwrap();
    assert_eq!(mismatch.range.start.line, 6);
    assert_eq!(mismatch.range.start.character, 15);
}

#[test]
fn annotated_nested_mismatches_keep_each_root_call_owner() {
    let engine = module_boundary_identity_engine();
    let fixture = nested_module_call_lsp_fixture(": aws.ec2.SecurityGroup.Id");
    let root_source = r#"let outer = use { source = '../outer' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

outer {
  vpc_id = main_vpc.vpc_id
}

outer {
  vpc_id = main_vpc.vpc_id
}
"#;
    std::fs::write(fixture.root.join("main.crn"), root_source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &fixture.root, "main.crn", root_source);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("argument 'vpc_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
        })
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 2, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        mismatches
            .iter()
            .map(|diagnostic| diagnostic.range)
            .collect::<Vec<_>>(),
        vec![
            Range::new(Position::new(6, 0), Position::new(8, 1)),
            Range::new(Position::new(10, 0), Position::new(12, 1)),
        ],
    );
}

#[test]
fn unannotated_nested_module_call_is_inferred_when_root_module_is_opened() {
    let engine = module_boundary_identity_engine();
    let fixture = nested_module_call_lsp_fixture("");

    let diagnostics = analyze_with_buffer(&engine, &fixture.root, "main.crn", &fixture.root_source);

    assert_one_nested_lsp_mismatch(&diagnostics, "instance.b", "instance.a.sg_id");
    let mismatch = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("module call 'instance.b'"))
        .unwrap();
    assert_eq!(mismatch.range.start.line, 6);
    assert_eq!(mismatch.range.start.character, 15);

    let sibling_source = "let sibling = aws.ec2.SecurityGroup { name = \"sibling\" }\n";
    std::fs::write(fixture.root.join("other.crn"), sibling_source).unwrap();
    let sibling_diagnostics =
        analyze_with_buffer(&engine, &fixture.root, "other.crn", sibling_source);
    assert!(
        sibling_diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("module call 'instance.b'")),
        "a transitive call diagnostic must remain owned by its root call document: {:?}",
        sibling_diagnostics
            .iter()
            .map(|diagnostic| (&diagnostic.range, &diagnostic.message))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn depth_three_unannotated_module_call_is_inferred_when_root_module_is_opened() {
    let engine = module_boundary_identity_engine();
    let temp = tempfile::tempdir().unwrap();
    let web_tier = temp.path().join("web_tier");
    let needs_vpc = temp.path().join("needs_vpc");
    let middle = temp.path().join("middle");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    for directory in [&web_tier, &needs_vpc, &middle, &outer, &root] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        web_tier.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_sg = aws.ec2.SecurityGroup {
  name   = "web"
  vpc_id = vpc_id
}

attributes {
  sg_id = web_sg.group_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        needs_vpc.join("main.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .unwrap();
    std::fs::write(
        middle.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_tier = use { source = '../web_tier' }
let needs_vpc = use { source = '../needs_vpc' }

let a = web_tier {
  vpc_id = vpc_id
}

let b = needs_vpc {
  vpc_id = a.sg_id
}
"#,
    )
    .unwrap();
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let middle = use { source = '../middle' }
let m = middle {
  vpc_id = vpc_id
}
"#,
    )
    .unwrap();
    let root_source = r#"let outer = use { source = '../outer' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let instance = outer {
  vpc_id = main_vpc.vpc_id
}
"#;
    std::fs::write(root.join("main.crn"), root_source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &root, "main.crn", root_source);
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'instance.m.b': argument 'vpc_id'")
                && diagnostic.message.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.message.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.message.contains("from instance.m.a.sg_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "the depth-three LSP boundary must be reported once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn no_undefined_resource_for_sibling_binding_in_exports() {
    let engine = DiagnosticEngine::new(
        loaded_test_providers(SchemaRegistry::new()),
        vec!["awscc".to_string()],
    );
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account { name = 'prod' }\nlet registry_dev = awscc.organizations.account { name = 'dev' }\n",
    )
    .unwrap();
    let exports = "exports {\n  accounts: map(AwsAccountId) = {\n    prod = registry_prod.account_id\n    dev = registry_dev.account_id\n  }\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);

    let undefined = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource"));
    assert!(
        undefined.is_none(),
        "Sibling binding refs should not be flagged as undefined. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_missing_source_directory_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(
        r#"let orgs = upstream_state {
    source = '../nonexistent'
}
"#,
        path.clone(),
    );

    let diagnostics = engine.analyze(&doc, Some(&path));

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state 'orgs'"));
    assert!(
        diag.is_some(),
        "Expected a diagnostic for missing upstream_state source. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let diag = diag.unwrap();
    assert!(
        diag.message.contains("../nonexistent") && diag.message.contains("does not exist"),
        "unexpected message: {}",
        diag.message
    );
    // Range should point at the source value on line 1 (0-based).
    assert_eq!(
        diag.range.start.line, 1,
        "diagnostic should point at the `source = ...` line"
    );
}

#[test]
fn upstream_state_source_check_ignores_same_source_in_provider_block() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();

    let engine = test_engine();
    // A `provider` block with the same `source = '...'` string as the
    // `upstream_state` block. The diagnostic must land on the upstream_state
    // line (line 5 here, 0-based 4), not the provider line.
    let path = base.join("main.crn");
    let doc = create_document_at(
        r#"provider aws {
    source = 'missing/provider'
    region = 'ap-northeast-1'
}
let orgs = upstream_state {
    source = 'missing/provider'
}
"#,
        path.clone(),
    );

    let diagnostics = engine.analyze(&doc, Some(&path));

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state 'orgs'"));
    assert!(
        diag.is_some(),
        "expected diagnostic for upstream_state source. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        diag.unwrap().range.start.line,
        5,
        "diagnostic must point at the upstream_state source line, not the provider's"
    );
}

#[test]
fn upstream_state_existing_source_directory_is_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("project");
    std::fs::create_dir(&base).unwrap();
    std::fs::create_dir(tmp.path().join("upstream")).unwrap();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(
        r#"let orgs = upstream_state {
    source = '../upstream'
}
"#,
        path.clone(),
    );

    let diagnostics = engine.analyze(&doc, Some(&path));

    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("upstream_state"));
    assert!(
        diag.is_none(),
        "Existing source should not trigger diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// =====================================================================
// upstream_state field-reference diagnostics (#1990)
// =====================================================================

fn set_up_project_with_upstream(
    main_crn: &str,
    upstream_exports_crn: Option<&str>,
) -> (tempfile::TempDir, std::path::PathBuf, String) {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    if let Some(body) = upstream_exports_crn {
        std::fs::write(upstream.join("exports.crn"), body).unwrap();
    }
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(base.join("main.crn"), main_crn).unwrap();
    (tmp, base, "main.crn".to_string())
}

fn analyze_with_buffer(
    engine: &DiagnosticEngine,
    base: &std::path::Path,
    filename: &str,
    buffer: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let path = base.join(filename);
    let doc = crate::document::Document::new_with_path(
        buffer.to_string(),
        path.clone(),
        std::sync::Arc::new(carina_core::parser::ProviderContext::default()),
    );
    engine.analyze(&doc, Some(&path))
}

fn shared_role_arn_directory() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let temp = tempfile::tempdir().expect("shared identity fixture");
    let module = temp.path().join("identity_center");
    let root = temp.path().join("root");
    std::fs::create_dir(&module).expect("module directory");
    std::fs::create_dir(&root).expect("root directory");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  role_arns: list(aws.iam.Role.Arn)\n}\n",
    )
    .expect("module arguments");
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  admin_access_role_arns: list(aws.iam.Role.Arn) = role_arns\n}\n",
    )
    .expect("module attributes");
    std::fs::write(
        root.join("providers.crn"),
        "provider aws {}\n\nprovider awscc {}\n",
    )
    .expect("root providers");
    std::fs::write(
        root.join("main.crn"),
        "let identity_center = use { source = '../identity_center' }\n\
         let admin = identity_center { role_arns = [\"arn:accepted\"] }\n",
    )
    .expect("root module call");
    let exports =
        "exports {\n  admin_access_role_arns: list(aws.iam.Role.Arn) = admin.admin_access_role_arns\n}\n"
            .to_string();
    std::fs::write(root.join("exports.crn"), &exports).expect("root exports");
    (temp, root, exports)
}

#[test]
fn shared_identity_annotation_is_owner_conjunctive_in_multi_file_lsp_analysis() {
    let (_temp, root, exports) = shared_role_arn_directory();
    let main = std::fs::read_to_string(root.join("main.crn")).expect("root main fixture");
    let accepted_engine = shared_role_arn_engine("arn:accepted");
    let mut accepted = analyze_with_buffer(&accepted_engine, &root, "main.crn", &main);
    accepted.extend(analyze_with_buffer(
        &accepted_engine,
        &root,
        "exports.crn",
        &exports,
    ));
    assert!(
        !accepted.iter().any(|diagnostic| {
            diagnostic.message.contains("ambiguous custom type")
                || diagnostic.message.contains("Unknown type")
                || diagnostic.message.contains("rejects")
        }),
        "both owners accepting the shared annotation must be diagnostic-free: {accepted:#?}"
    );

    let rejected = analyze_with_buffer(
        &shared_role_arn_engine("arn:other"),
        &root,
        "main.crn",
        &main,
    );
    assert!(
        rejected
            .iter()
            .any(|diagnostic| diagnostic.message.contains("awscc rejects")),
        "one rejecting owner must reject the shared annotation: {rejected:#?}"
    );
}

#[test]
fn registered_bare_provider_type_does_not_remain_a_fail_open_simple_in_lsp() {
    let directory = tempfile::tempdir().expect("bare type fixture");
    std::fs::write(directory.path().join("providers.crn"), "provider test {}\n")
        .expect("provider fixture");
    let exports = "exports {\n  token: TestToken = 'anything'\n}\n";
    std::fs::write(directory.path().join("exports.crn"), exports).expect("exports fixture");

    let diagnostics = analyze_with_buffer(
        &bare_type_engine(),
        directory.path(),
        "exports.crn",
        exports,
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("bare token rejected by its owner")),
        "the registered bare type must route to its provider owner: {diagnostics:#?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Unknown type")),
        "the registered bare type must resolve before validation: {diagnostics:#?}"
    );
}

#[test]
fn upstream_state_unknown_field_in_for_expression_is_flagged() {
    // The issue's canonical repro.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in orgs.account {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `account`")),
        "got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_missing_coalesce_value_is_not_flagged() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name in (orgs.account ?? []) {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: list(String) = ["x"] }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not export `account`")),
        "a missing coalesce value is intentionally optional: {diagnostics:#?}",
    );
}

#[test]
fn upstream_state_nested_coalesce_lhs_reference_is_still_flagged() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name in (["${orgs.account}-suffix"] ?? []) {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: list(String) = ["x"] }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not export `account`")),
        "a reference nested inside an interpolation is required even on the left of ??: {diagnostics:#?}",
    );
}

#[test]
fn upstream_state_missing_coalesce_default_is_still_flagged() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name in (orgs.accounts ?? orgs.account) {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: list(String) = ["x"] }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not export `account`")),
        "references in the coalesce default remain required: {diagnostics:#?}",
    );
}

fn coalesce_resource_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    custom_engine(schemas)
}

#[test]
fn resource_attribute_coalesce_rejects_wrong_typed_default() {
    let source = r#"provider mock {}

let producer = mock.test.resource {
  name = "seed"
}

let consumer = mock.test.resource {
  name = "consumer"
  comment = producer.comment ?? 42
}
"#;
    let diagnostics = coalesce_resource_engine().analyze(&create_document(source), None);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            let message = diagnostic.message.to_ascii_lowercase();
            message.contains("string") && message.contains("int")
        }),
        "the LSP must report a coalesce default that conflicts with the resource attribute schema: {diagnostics:#?}",
    );
}

#[test]
fn for_iterable_coalesce_rejects_wrong_typed_default() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let source = r#"provider mock {}

let producer = mock.test.resource {
  name = "seed"
}

for key, value in (producer.tags ?? 5) {
  mock.test.resource {
    name = "${key}-${value}"
  }
}
"#;
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.crn"), source).unwrap();
    let path = fixture.path().join("main.crn");
    let diagnostics =
        custom_engine(schemas).analyze(&create_document_at(source, path.clone()), Some(&path));

    assert!(
        diagnostics.iter().any(|diagnostic| {
            let message = diagnostic.message.to_ascii_lowercase();
            message.contains("map") && message.contains("int")
        }),
        "the LSP must reject a coalesce default incompatible with the for iterable: {diagnostics:#?}",
    );
}

#[test]
fn resource_attribute_typo_on_coalesce_lhs_is_still_flagged() {
    let source = r#"provider mock {}

let producer = mock.test.resource {
  name = "seed"
}

let consumer = mock.test.resource {
  name = producer.cmment ?? "d"
}
"#;
    let diagnostics = coalesce_resource_engine().analyze(&create_document(source), None);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("cmment")
                && diagnostic
                    .message
                    .to_ascii_lowercase()
                    .contains("unknown attribute")
        }),
        "the LSP must not suppress schema-typed unknown attributes on a coalesce lhs: {diagnostics:#?}",
    );
}

fn field_group_diagnostic_engine(
    list: bool,
    kind: carina_core::schema::FieldGroupKind,
) -> DiagnosticEngine {
    use carina_core::schema::{
        AttributeSchema, AttributeType, FieldGroup, ResourceSchema, StructField,
    };

    let choice = AttributeType::struct_with_field_groups(
        "EvaluationWindow",
        vec![
            StructField::optional("sliding_window", AttributeType::string()).build(),
            StructField::optional("wall_clock_window", AttributeType::string()).build(),
        ],
        vec![FieldGroup::new(kind, ["sliding_window", "wall_clock_window"]).unwrap()],
    );
    let field_type = AttributeType::ref_("EvaluationWindow");
    let attribute = if list {
        AttributeSchema::optional("evaluation_window", AttributeType::list(field_type))
            .with_block_name("evaluation_window")
            .build()
    } else {
        AttributeSchema::optional("evaluation_window", field_type).build()
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .with_def("EvaluationWindow", choice)
            .attribute(attribute)
            .validate()
            .unwrap(),
    );
    custom_engine(schemas)
}

#[test]
fn struct_field_group_diagnostic_is_emitted_once_on_the_containing_attribute() {
    let source = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_window = {}\n",
        "}\n",
    );
    let diagnostics =
        field_group_diagnostic_engine(false, carina_core::schema::FieldGroupKind::ExactlyOne)
            .analyze(&create_document(source), None);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("exactly one of"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].message,
        "evaluation_window: exactly one of [sliding_window, wall_clock_window] must be set"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(3, 2), Position::new(3, 19))
    );
}

#[test]
fn list_struct_field_group_diagnostic_is_emitted_once_on_the_containing_block() {
    let source = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_window {\n",
        "  }\n",
        "}\n",
    );
    let diagnostics =
        field_group_diagnostic_engine(true, carina_core::schema::FieldGroupKind::ExactlyOne)
            .analyze(&create_document(source), None);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("exactly one of"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].message,
        "evaluation_window[0]: exactly one of [sliding_window, wall_clock_window] must be set"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(3, 2), Position::new(3, 19))
    );
}

#[test]
fn lsp_enforces_at_least_one_and_at_most_one_field_groups() {
    let missing = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_window = {}\n",
        "}\n",
    );
    let diagnostics =
        field_group_diagnostic_engine(false, carina_core::schema::FieldGroupKind::AtLeastOne)
            .analyze(&create_document(missing), None);
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("at least one of"))
            .count(),
        1,
        "diagnostics: {diagnostics:#?}"
    );

    let both = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_window = {\n",
        "    sliding_window = \"5m\"\n",
        "    wall_clock_window = \"hour\"\n",
        "  }\n",
        "}\n",
    );
    let diagnostics =
        field_group_diagnostic_engine(false, carina_core::schema::FieldGroupKind::AtMostOne)
            .analyze(&create_document(both), None);
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("at most one of"))
            .count(),
        1,
        "diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn union_ref_struct_field_group_diagnostic_names_the_attribute_once() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, FieldGroup, ResourceSchema, StructField,
    };

    let definition = AttributeType::struct_with_field_groups(
        "EvaluationWindow",
        vec![
            StructField::optional("sliding_window", AttributeType::string()).build(),
            StructField::optional("wall_clock_window", AttributeType::string()).build(),
        ],
        vec![FieldGroup::exactly_one(["sliding_window", "wall_clock_window"]).unwrap()],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .with_def("EvaluationWindow", definition)
            .attribute(
                AttributeSchema::optional(
                    "evaluation_window",
                    AttributeType::union(vec![
                        AttributeType::ref_("EvaluationWindow"),
                        AttributeType::string(),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_window = {}\n",
        "}\n",
    );
    let diagnostics = custom_engine(schemas).analyze(&create_document(source), None);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("exactly one of"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].message,
        "evaluation_window: exactly one of [sliding_window, wall_clock_window] must be set"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(3, 2), Position::new(3, 19))
    );
}

#[test]
fn map_ref_struct_field_group_diagnostic_anchors_the_map_entry_once() {
    use carina_core::schema::{
        AttributeSchema, AttributeType, FieldGroup, ResourceSchema, StructField,
    };

    let definition = AttributeType::struct_with_field_groups(
        "EvaluationWindow",
        vec![
            StructField::optional("sliding_window", AttributeType::string()).build(),
            StructField::optional("wall_clock_window", AttributeType::string()).build(),
        ],
        vec![FieldGroup::exactly_one(["sliding_window", "wall_clock_window"]).unwrap()],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .with_def("EvaluationWindow", definition)
            .attribute(
                AttributeSchema::optional(
                    "evaluation_windows",
                    AttributeType::map(AttributeType::ref_("EvaluationWindow")),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source = concat!(
        "provider mock {}\n\n",
        "mock.test.resource {\n",
        "  evaluation_windows = {\n",
        "    primary = {}\n",
        "  }\n",
        "}\n",
    );
    let diagnostics = custom_engine(schemas).analyze(&create_document(source), None);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("exactly one of"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].message,
        "evaluation_windows.primary: exactly one of [sliding_window, wall_clock_window] must be set"
    );
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(4, 4), Position::new(4, 11))
    );
}

#[test]
fn coalesce_chain_is_accepted_by_lsp_diagnostics() {
    let source = r#"provider mock {}

let producer = mock.test.resource {
  name = "seed"
}

let consumer = mock.test.resource {
  name = "consumer"
  comment = producer.comment ?? producer.identifier ?? "d"
}
"#;
    let diagnostics = coalesce_resource_engine().analyze(&create_document(source), None);

    assert!(
        diagnostics.is_empty(),
        "the LSP parser and diagnostics must accept a coalesce chain: {diagnostics:#?}",
    );
}

#[test]
fn upstream_state_known_field_passes() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in orgs.accounts {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("does not export")),
        "known field should not be flagged, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_typed_struct_missing_field_is_reported_once() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let vpc = awscc.ec2.Vpc {
    name = orgs.account.nope
    cidr_block = '10.0.0.0/16'
}
"#,
        Some(
            r#"exports {
    account: struct { id: String } = { id = "account-123" }
}
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);
    let missing: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("nope"))
        .collect();

    assert_eq!(missing.len(), 1, "{diagnostics:#?}");
}

#[test]
fn upstream_state_buffer_differs_from_disk_uses_buffer() {
    // Disk: `orgs.accounts` (correct). Buffer in editor: `orgs.acc` (typo).
    // Must flag based on the buffer, not disk.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let edited = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.acc
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, edited);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `acc`")),
        "buffer typo must be flagged against disk exports, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_buffer_fix_clears_diagnostic() {
    // Disk: `orgs.account` (typo, would be flagged). Buffer: `orgs.accounts`
    // (user fixed it). Must NOT flag — reflects the buffer, not disk.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.account
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let fixed = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, fixed);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("does not export")),
        "fixed buffer should clear diagnostic even if disk is stale, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_buffer_retypo_reflags() {
    // Reproduces the user-reported sequence:
    //   disk typo → fix in buffer → retype typo in buffer.
    // Diagnostic must reappear on the second typo.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.accounts
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();

    // Simulate user editing to a new typo.
    let retypo = r#"let orgs = upstream_state { source = '../organizations' }
let x = orgs.acc
"#;
    let diagnostics = analyze_with_buffer(&engine, &base, &name, retypo);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `acc`")),
        "retyped typo must reflag, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_cross_file_declaration_is_checked() {
    // `let orgs = upstream_state { ... }` lives in `backend.crn`; the
    // reference lives in `main.crn`. Must still flag.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        r#"exports { accounts: String = "x" }
"#,
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("backend.crn"),
        r#"let orgs = upstream_state { source = '../organizations' }
"#,
    )
    .unwrap();
    let main_src = r#"for name, _ in orgs.account {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#;
    std::fs::write(base.join("main.crn"), main_src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main_src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("does not export `account`")),
        "cross-file upstream_state ref must be flagged, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_duplicate_bad_refs_anchor_to_distinct_sites() {
    // Two for-iterables both misspell the same field. Each diagnostic
    // must anchor to its own source site, not stack on the first.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state {
    source = '../organizations'
}

for name, _ in orgs.bad {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}

for other, _ in orgs.bad {
    awscc.ec2.Subnet {
        name = other
        cidr_block = '10.0.1.0/24'
    }
}
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    let bad_ref_diags: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("does not export `bad`"))
        .collect();
    assert_eq!(
        bad_ref_diags.len(),
        2,
        "expected 2 diagnostics for 2 occurrences, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let lines: std::collections::HashSet<u32> =
        bad_ref_diags.iter().map(|d| d.range.start.line).collect();
    assert_eq!(
        lines.len(),
        2,
        "diagnostics should anchor to distinct lines, got lines: {:?}",
        bad_ref_diags
            .iter()
            .map(|d| d.range.start.line)
            .collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_single_line_block_source_diagnostic() {
    // `let orgs = upstream_state { source = '../x' }` on one line.
    // `find_source_value_position` must still locate the source value.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("main.crn"), "not valid crn {{{").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    let src = "let orgs = upstream_state { source = '../organizations' }\n";
    std::fs::write(base.join("main.crn"), src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("failed to parse source")),
        "single-line upstream_state block must yield resolve-error diagnostic, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn upstream_state_broken_upstream_surfaces_resolve_error() {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(upstream.join("main.crn"), "not valid crn {{{").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    let src = r#"let orgs = upstream_state {
    source = '../organizations'
}
"#;
    std::fs::write(base.join("main.crn"), src).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", src);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("failed to parse source")),
        "broken upstream must produce resolve-error diagnostic, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// =====================================================================
// for-expression iterable: undefined binding (#1998)
// =====================================================================

#[test]
fn for_iterable_undefined_binding_is_flagged() {
    // `org` is a typo for `orgs` — the binding doesn't exist. LSP must
    // flag it, the same way `let x = org.accounts` outside a `for` does.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in org.accounts {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined identifier `org`")),
        "got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_coalesce_default_undefined_binding_is_flagged() {
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name in (orgs.accounts ?? fallback.accounts) {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: list(String) = ["x"] }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined identifier `fallback`")),
        "the coalesce default must be walked, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_defined_binding_passes() {
    // `orgs` is correct — no undefined-identifier diagnostic.
    let (_tmp, base, name) = set_up_project_with_upstream(
        r#"let orgs = upstream_state { source = '../organizations' }
for name, _ in orgs.accounts {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#,
        Some(
            r#"exports { accounts: String = "x" }
"#,
        ),
    );

    let engine = test_engine();
    let buffer = std::fs::read_to_string(base.join(&name)).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, &name, &buffer);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined identifier")),
        "correctly-named binding should not flag, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_undefined_binding_cross_file_is_flagged() {
    // `orgs` is declared in sibling file, but main.crn typos it as `org`.
    // The directory-scoped merge must still catch it.
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        r#"exports { accounts: String = "x" }
"#,
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("upstream.crn"),
        r#"let orgs = upstream_state { source = '../organizations' }
"#,
    )
    .unwrap();
    let main = r#"for name, _ in org.accounts {
    awscc.ec2.Vpc {
        name = name
        cidr_block = '10.0.0.0/16'
    }
}
"#;
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined identifier `org`")),
        "cross-file typo must be flagged after directory merge, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_undefined_binding_not_duplicated_across_siblings() {
    // Two sibling files with `for _ in <same_undefined>.<attr>` on the same
    // 1-based line must produce exactly one diagnostic in the currently
    // edited file — not one per deferred in the merged parse.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("project");
    std::fs::create_dir(&base).unwrap();
    let main = r#"for name, _ in missing.accounts {
    awscc.ec2.Vpc { name = name cidr_block = '10.0.0.0/16' }
}
"#;
    let sibling = r#"for name, _ in missing.accounts {
    awscc.ec2.Vpc { name = name cidr_block = '10.0.0.0/16' }
}
"#;
    std::fs::write(base.join("main.crn"), main).unwrap();
    std::fs::write(base.join("sibling.crn"), sibling).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Undefined identifier `missing`"))
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one diagnostic in main.crn, got {}: {:?}",
        hits.len(),
        hits.iter()
            .map(|d| (&d.range, &d.message))
            .collect::<Vec<_>>()
    );
}

#[test]
fn for_iterable_multiline_header_still_flagged() {
    // Pest's `WHITESPACE` includes NEWLINE, so `for _ in\n    bad.attr {`
    // parses. `deferred.line` points at the `for` keyword's line, which
    // doesn't contain the iterable token — the diagnostic must still surface
    // (anchored at the `for` line) rather than be silently dropped.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("project");
    std::fs::create_dir(&base).unwrap();
    let main = "for name, _ in\n    missing.accounts {\n    awscc.ec2.Vpc { name = name cidr_block = '10.0.0.0/16' }\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined identifier `missing`")),
        "multi-line for header must still flag the undefined iterable, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn lsp_binding_used_only_in_for_body_is_not_flagged_unused() {
    let provider = test_engine();
    let source = r#"
let vpc = test.r.vpc { name = "v" }
for _, id in orgs.xs {
  test.r.res { name = vpc.name }
}
"#;
    let doc = create_document(source);
    let diagnostics = provider.analyze(&doc, None);
    // Match "Unused" (LSP check_unused_bindings) and "unused" (parser warnings)
    // case-insensitively so the assertion fires whichever path flags `vpc`.
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.to_lowercase().contains("unused") && d.message.contains("vpc")),
        "vpc used in for body, must not be flagged unused, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn single_quoted_interpolation_like_sequence_emits_warning_at_span() {
    let engine = test_engine();
    let buffer = "let env = \"prod\"\nlet policy = 'arn:${env}:${account}:root'\n";
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let warnings: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("single-quoted string contains '${...}'"))
        .collect();

    assert_eq!(
        warnings.len(),
        2,
        "expected one single-quoted interpolation warning per occurrence; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let line = buffer.lines().nth(1).unwrap();
    let first_start = line.find("${env}").unwrap() as u32;
    let end = (line.find("${env}").unwrap() + "${env}".len()) as u32;
    assert_eq!(
        warnings[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(warnings[0].range.start.line, 1);
    assert_eq!(warnings[0].range.start.character, first_start);
    assert_eq!(warnings[0].range.end.character, end);

    let second_start = line.find("${account}").unwrap() as u32;
    let second_end = (line.find("${account}").unwrap() + "${account}".len()) as u32;
    assert_eq!(
        warnings[1].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(warnings[1].range.start.line, 1);
    assert_eq!(warnings[1].range.start.character, second_start);
    assert_eq!(warnings[1].range.end.character, second_end);
}

#[test]
fn non_literal_prevent_destroy_warning_is_directory_scoped() {
    let fixture = tempfile::tempdir().expect("multi-file diagnostics fixture");
    let resource = r#"let account = mock.test.resource {
  name = "example"
  directives { prevent_destroy = allow_destroy }
}
"#;
    std::fs::write(
        fixture.path().join("flags.crn"),
        "let allow_destroy = false\n",
    )
    .expect("write sibling expression input");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
        .expect("write sibling provider fixture");
    std::fs::write(fixture.path().join("resources.crn"), resource).expect("write resource fixture");

    let path = fixture.path().join("resources.crn");
    let diagnostics =
        test_engine().analyze(&create_document_at(resource, path.clone()), Some(&path));
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("only a literal `false` opts out of schema-default protection")
        })
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "diagnostics: {diagnostics:#?}");
    let warning = warnings[0];

    assert_eq!(
        warning.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(warning.range.start.line, 2);
}

#[test]
fn sibling_true_prevent_destroy_expression_does_not_warn() {
    let fixture = tempfile::tempdir().expect("multi-file diagnostics fixture");
    let resource = r#"let account = mock.test.resource {
  name = "example"
  directives { prevent_destroy = protect }
}
"#;
    std::fs::write(fixture.path().join("flags.crn"), "let protect = true\n")
        .expect("write sibling expression input");
    std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
        .expect("write sibling provider fixture");
    std::fs::write(fixture.path().join("resources.crn"), resource).expect("write resource fixture");

    let path = fixture.path().join("resources.crn");
    let diagnostics =
        test_engine().analyze(&create_document_at(resource, path.clone()), Some(&path));
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("only a literal `false` opts out of schema-default protection")
        })
        .collect::<Vec<_>>();

    assert!(
        warnings.is_empty(),
        "the directory-resolved true value enables protection without an opt-out warning: {diagnostics:#?}"
    );
}

#[test]
fn single_quoted_interpolation_warning_uses_original_line_after_heredoc() {
    let engine = test_engine();
    let buffer = r#"let doc = <<EOF
alpha
beta
EOF
let env = "prod"
let policy = 'arn:${env}:root'
"#;
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let warning = diagnostics
        .iter()
        .find(|d| d.message.contains("single-quoted string contains '${...}'"))
        .unwrap_or_else(|| {
            panic!(
                "expected a single-quoted interpolation warning; got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });

    assert_eq!(
        warning.range.start.line, 5,
        "LSP diagnostics are zero-based and should use original source lines"
    );
}

#[test]
fn single_quoted_interpolation_like_sequence_negative_cases_do_not_warn() {
    let engine = test_engine();
    let buffer = r#"
let env = "prod"
let double_quoted = "arn:${env}:root"
let plain_single = 'arn:env:root'
let lone_dollar = '$'
let dollar_name = '$env'
let missing_close = '${env'
"#;
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.message.contains("single-quoted string contains '${...}'")),
        "negative cases must not warn, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn enum_mismatch_inside_for_body_surfaces_as_diagnostic() {
    let provider = test_engine_with_enum_attr();
    let source = r#"
for _, id in orgs.xs {
  test.r.mode_holder {
    mode = "aaaa"
  }
}
"#;
    let doc = create_document(source);
    let parsed = doc.parsed().expect("buffer fixture parses");
    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);
    let diagnostics = provider.analyze(&doc, None);
    assert!(
        diagnostics.iter().any(|d| d.message.contains("aaaa")),
        "expected enum-mismatch diagnostic inside for body, got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn for_body_diagnostic_anchors_inside_for_not_at_prior_sibling() {
    // Regression for #2078. The same attribute name (`mode`) appears both at
    // top level (valid) and inside a `for` body (invalid). The diagnostic
    // must anchor on the for-body line, not the earlier top-level line that
    // happens to share the attribute name.
    let provider = test_engine_with_enum_attr();
    // Line layout (0-indexed):
    //   0: (empty)
    //   1: test.r.mode_holder {
    //   2:   mode = "fast"           <- valid
    //   3: }
    //   4:
    //   5: for _, id in orgs.xs {
    //   6:   test.r.mode_holder {
    //   7:     mode = "aaaa"         <- INVALID, should anchor here
    //   8:   }
    //   9: }
    let source = r#"
test.r.mode_holder {
  mode = "fast"
}

for _, id in orgs.xs {
  test.r.mode_holder {
    mode = "aaaa"
  }
}
"#;
    let doc = create_document(source);
    let diagnostics = provider.analyze(&doc, None);
    let bad = diagnostics
        .iter()
        .find(|d| d.message.contains("aaaa"))
        .unwrap_or_else(|| {
            panic!(
                "expected enum-mismatch diagnostic for 'aaaa', got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        bad.range.start.line, 7,
        "diagnostic must anchor on the for-body line (0-indexed 7), got: line {} with range {:?}",
        bad.range.start.line, bad.range
    );
}

#[test]
fn lsp_enum_diagnostic_includes_attribute_name() {
    // Regression for #2098. The LSP enum-mismatch diagnostic must quote
    // the attribute name (e.g. `'mode'`) so a reader can locate the bad
    // token in their file when the same enum type appears on several
    // attributes.
    let provider = test_engine_with_enum_attr();
    let doc = create_document(
        r#"test.r.mode_holder {
  mode = "aaaa"
}
"#,
    );
    let diagnostics = provider.analyze(&doc, None);
    let bad = diagnostics
        .iter()
        .find(|d| d.message.contains("aaaa"))
        .unwrap_or_else(|| {
            panic!(
                "expected enum-mismatch diagnostic, got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
    assert!(
        bad.message.contains("'mode'"),
        "diagnostic should name the attribute, got: {}",
        bad.message
    );
}

/// Regression for #2094: the LSP must mirror the CLI's
/// `StringLiteralExpectedEnum` diagnostic when the user writes
/// `mode = "aaa"` against a namespaced `Enum` attribute.
/// See PR 2 (#2112) for the CLI side.
#[test]
fn lsp_quoted_literal_on_namespaced_enum_says_got_a_string_literal() {
    let provider = test_engine_with_namespaced_enum_attr();
    let doc = create_document(
        r#"test.r.mode_holder {
  mode = "aaa"
}
"#,
    );
    let diagnostics = provider.analyze(&doc, None);
    let bad = diagnostics
        .iter()
        .find(|d| d.message.contains("got a string literal"))
        .unwrap_or_else(|| {
            panic!(
                "expected shape-mismatch diagnostic for quoted literal on enum attribute, got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
    assert!(
        bad.message.contains("'aaa'"),
        "diagnostic must echo the user's literal, got: {}",
        bad.message
    );
    assert!(
        bad.message.contains("'mode'"),
        "diagnostic must name the attribute, got: {}",
        bad.message
    );
    assert!(
        bad.message.contains("test.r.Mode.fast") || bad.message.contains("test.r.Mode.slow"),
        "diagnostic must list valid variants, got: {}",
        bad.message
    );
}

/// Bare-identifier mistake on the same namespaced enum must keep the
/// classic InvalidEnumVariant wording — no shape-mismatch phrasing.
#[test]
fn lsp_bare_invalid_on_namespaced_enum_keeps_invalid_variant_wording() {
    let provider = test_engine_with_namespaced_enum_attr();
    let doc = create_document(
        r#"test.r.mode_holder {
  mode = aaa
}
"#,
    );
    let diagnostics = provider.analyze(&doc, None);
    let diag = diagnostics
        .iter()
        .find(|d| d.message.contains("aaa"))
        .unwrap_or_else(|| {
            panic!(
                "expected diagnostic for bare invalid identifier, got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
    assert!(
        !diag.message.contains("got a string literal"),
        "bare identifier must NOT trigger the shape-mismatch wording, got: {}",
        diag.message
    );
}

/// Regression for #2094 Custom-type case: a namespaced `AttributeType::Custom`
/// written as `mode = "aaa"` must emit a shape-mismatch diagnostic just
/// like `Enum`.
#[test]
fn lsp_quoted_literal_on_namespaced_custom_says_got_a_string_literal() {
    let provider = test_engine_with_custom_namespaced_attr();
    let doc = create_document(
        r#"test.r.mode_holder {
  mode = "aaa"
}
"#,
    );
    let diagnostics = provider.analyze(&doc, None);
    let bad = diagnostics
        .iter()
        .find(|d| d.message.contains("got a string literal"))
        .unwrap_or_else(|| {
            panic!(
                "expected shape-mismatch diagnostic for quoted literal on Custom namespaced attribute, got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
    assert!(
        bad.message.contains("'aaa'"),
        "diagnostic must echo the user's literal, got: {}",
        bad.message
    );
    assert!(
        bad.message.contains("'mode'"),
        "diagnostic must name the attribute, got: {}",
        bad.message
    );
}

#[test]
fn lsp_custom_string_pattern_mismatch_reports_required_pattern() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, legacy_validator};

    let bucket_name = AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("BucketName"),
        Some("^prod-[a-z0-9]+$".to_string()),
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let schema = ResourceSchema::new("s3.bucket")
        .attribute(AttributeSchema::optional("bucket_name", bucket_name).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let doc = create_document(
        r#"aws.s3.bucket {
  bucket_name = "dev_bucket"
}
"#,
    );
    let diagnostics = engine.analyze(&doc, None);
    let pattern_mismatch = diagnostics
        .iter()
        .find(|d| d.message.contains("does not match required pattern"))
        .unwrap_or_else(|| {
            panic!(
                "expected pattern mismatch diagnostic, got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });

    assert!(
        pattern_mismatch.message.contains("^prod-[a-z0-9]+$"),
        "diagnostic must include the schema pattern, got: {}",
        pattern_mismatch.message
    );
}

#[test]
fn lsp_secret_value_constraint_diagnostic_is_masked() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let plaintext = "lsp-secret-plaintext";
    let schema = ResourceSchema::new("test.SecretHolder").attribute(
        AttributeSchema::optional(
            "password",
            AttributeType::refined_string(None, None, Some((Some(2), Some(4))), None),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(&format!(
        "test.test.SecretHolder {{\n  password = secret('{plaintext}')\n}}\n"
    ));

    let diagnostics = engine.analyze(&doc, None);
    let rendered = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("outside allowed range"),
        "diagnostics: {rendered}"
    );
    assert!(rendered.contains("(secret)"), "diagnostics: {rendered}");
    assert!(
        !rendered.contains(plaintext),
        "secret leaked in LSP diagnostic: {rendered}"
    );
}

#[test]
fn lsp_nested_secret_value_constraint_diagnostic_is_masked() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let plaintext = "nested-lsp-secret-plaintext";
    let schema = ResourceSchema::new("test.SecretHolder").attribute(
        AttributeSchema::optional(
            "passwords",
            AttributeType::list(AttributeType::refined_string(
                None,
                None,
                Some((Some(2), Some(4))),
                None,
            )),
        )
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(&format!(
        "test.test.SecretHolder {{\n  passwords = [secret('{plaintext}')]\n}}\n"
    ));

    let diagnostics = engine.analyze(&doc, None);
    let rendered = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("(secret)"), "diagnostics: {rendered}");
    assert!(
        !rendered.contains(plaintext),
        "nested secret leaked in LSP diagnostic: {rendered}"
    );
}

// #2131 / #2132: a ResourceRef whose root is declared in a sibling `.crn`
// must NOT be flagged `Undefined resource`. The LSP text-scan used to
// feed on the current file's bindings plus a hand-rolled sibling scan
// that skipped `upstream_state`, `import`, and module-call bindings.
// Merged-parse is the source of truth.
#[test]
fn upstream_state_binding_in_sibling_file_is_not_undefined() {
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("organizations");
    std::fs::create_dir(&upstream).unwrap();
    std::fs::write(
        upstream.join("exports.crn"),
        "exports {\n  accounts: map(AwsAccountId) = \"x\"\n}\n",
    )
    .unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("backend.crn"),
        "let orgs = upstream_state { source = '../organizations' }\n",
    )
    .unwrap();
    // The #2131 repro: `orgs.accounts` on the RHS of an assignment.
    // The text-scan `check_undefined_references` only inspects lines
    // that contain `=`, so the bug surfaces inside a resource block.
    let main = "awscc.ec2.Vpc {\n  target_id = orgs.accounts\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    let undefined: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("Undefined resource") || d.message.contains("Undefined identifier")
        })
        .collect();
    assert!(
        undefined.is_empty(),
        "sibling-declared `upstream_state` binding must not be flagged. Got: {:?}",
        undefined.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// `import` bindings in a sibling file were also invisible to the old
// text-scan (no `provider.service.type` RHS). Cover that shape too.
#[test]
fn import_binding_in_sibling_file_is_not_undefined() {
    let tmp = tempfile::tempdir().unwrap();
    let module = tmp.path().join("modules").join("vpc");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(module.join("main.crn"), "arguments {\n  name: String\n}\n").unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    std::fs::write(
        base.join("imports.crn"),
        "let vpc_mod = use { source = '../modules/vpc' }\n",
    )
    .unwrap();
    let main = "awscc.ec2.Vpc {\n  name = vpc_mod.name\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    let undefined: Vec<_> = diagnostics
        .iter()
        .filter(|d| {
            d.message.contains("Undefined resource") || d.message.contains("Undefined identifier")
        })
        .collect();
    assert!(
        undefined.is_empty(),
        "sibling-declared `import` binding must not be flagged. Got: {:?}",
        undefined.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// Genuine undefined still fires (typo against no declared binding at all).
#[test]
fn undefined_binding_in_current_file_still_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir(&base).unwrap();
    let main = "awscc.ec2.Vpc {\n  name = nowhere.value\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Undefined resource")
                || d.message.contains("Undefined identifier")),
        "typo against a truly undeclared root must still be flagged. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// #2134: a resource `let` binding used only from a sibling `.crn`
// inside a list literal must not be flagged as unused. The old
// `scan_sibling_context::referenced` text scan only matched lines
// with `=` where the RHS *starts with* `binding_name.`; a reference
// nested inside `[ binding.field, ... ]` or `func(binding.field)`
// misses that heuristic.
#[test]
fn binding_referenced_only_from_sibling_list_literal_is_not_unused() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let admin_group = awscc.identitystore.group {\n  display_name = 'admins'\n  identity_store_id = 'd-xxx'\n}\n",
    )
    .unwrap();
    // The sibling references `admin_group` inside a list literal —
    // the text scan's "starts with `admin_group.`" heuristic misses it.
    std::fs::write(
        base.join("policy.crn"),
        "awscc.iam.policy {\n  name = 'admins-policy'\n  principals = [admin_group.group_id]\n}\n",
    )
    .unwrap();

    let engine = test_engine();
    let main_src = std::fs::read_to_string(base.join("main.crn")).unwrap();
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", &main_src);

    let unused: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("Unused let binding") && d.message.contains("admin_group"))
        .collect();
    assert!(
        unused.is_empty(),
        "binding referenced from sibling list literal must not be flagged unused. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// Unused-binding detection still fires when no sibling references
// the binding at all.
#[test]
fn unreferenced_binding_is_still_flagged_unused() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    let main =
        "let orphan = awscc.ec2.Vpc {\n  name = 'stranded'\n  cidr_block = '10.0.0.0/16'\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();
    std::fs::write(
        base.join("other.crn"),
        "awscc.s3.Bucket {\n  name = 'unrelated'\n}\n",
    )
    .unwrap();

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (ResourceSchema::new("s3.Bucket").attribute(
            AttributeSchema::optional("name", AttributeType::string())
                .create_only()
                .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let diagnostics = analyze_with_buffer(&engine, &base, "main.crn", main);

    assert!(
        diagnostics
            .iter()
            .any(|d| d.message.contains("Unused let binding") && d.message.contains("orphan")),
        "truly unused binding must still fire. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// #2134: `exports { key: T = sibling_binding.attr }` where
// `sibling_binding` is declared in a sibling `.crn` as
// `upstream_state`, `import`, or a resource must get a correct type
// mismatch diagnostic (not silently accepted, not falsely flagged).
// The old `scan_sibling_context` only returned a `HashMap<binding,
// resource_type>` for bindings declared as `let x = provider.service.type {`.
// For the `let x = <upstream_state|import>{}` shapes the map was
// empty, so the type check short-circuited via the "can't resolve"
// fall-through.
//
// This test isn't asserting a new diagnostic — it asserts no
// regression: the existing "type mismatch" path still works when the
// sibling binding is a real resource declared in a sibling file.
#[test]
fn exports_type_check_resolves_resource_binding_from_sibling_file() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    // Schema: registry_prod.account_id is String.
    let schema = ResourceSchema::new("organizations.account")
        .attribute(AttributeSchema::optional("account_id", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    // Resource binding lives in main.crn.
    std::fs::write(
        base.join("main.crn"),
        "let registry_prod = awscc.organizations.account {\n  name = 'prod'\n}\n",
    )
    .unwrap();
    // exports.crn references it. `map(bool)` is incompatible with
    // String, so we expect a type-mismatch warning — which is what
    // proves the sibling-binding resolution actually happened.
    let exports =
        "exports {\n  accounts: map(Bool) = {\n    prod = registry_prod.account_id\n  }\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);
    assert!(
        diagnostics
            .iter()
            .any(|d| { d.message.contains("type mismatch") || d.message.contains("expected") }),
        "type-mismatch warning should fire for map(Bool) vs String. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn exports_ref_type_mismatch_displays_resolved_schema_name() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let schema = ResourceSchema::new("source.Item")
        .attribute(AttributeSchema::optional("value", AttributeType::ref_("S".to_string())).build())
        .with_def("S", AttributeType::string());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("downstream");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let source = test.source.Item {}\n").unwrap();
    let exports = "exports {\n  value: Bool = source.value\n}\n";
    std::fs::write(base.join("exports.crn"), exports).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "exports.crn", exports);
    let mismatch_messages: Vec<&str> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .filter(|message| message.contains("type mismatch") && message.contains("source.value"))
        .collect();
    assert!(
        mismatch_messages
            .iter()
            .any(|message| message.contains("expected Bool, got String")),
        "resolved String mismatch should be reported: {mismatch_messages:?}",
    );
    assert!(
        mismatch_messages
            .iter()
            .all(|message| !message.contains("Ref(")),
        "all LSP mismatch surfaces should hide schema-internal Refs: {mismatch_messages:?}",
    );
}

// #3716: attributes diagnostics must use the directory-merged parse for
// binding authority while continuing to anchor diagnostics in the open buffer.
fn attributes_sibling_resource_engine() -> DiagnosticEngine {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity, legacy_validator,
    };

    let target_group_schema = ResourceSchema::new("stub.TargetGroup")
        .attribute(AttributeSchema::optional("target_group_arn", AttributeType::string()).build());
    let vpc_id_type = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let subnet_id_type = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("SubnetId"),
        None,
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let vpc_schema = ResourceSchema::new("stub.Vpc")
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_type).build())
        .attribute(AttributeSchema::optional("subnet_id", subnet_id_type).build())
        .attribute(
            AttributeSchema::optional(
                "config",
                AttributeType::struct_(
                    "VpcConfig",
                    vec![StructField::optional("name", AttributeType::string()).build()],
                ),
            )
            .build(),
        );
    let arn_type = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Arn"),
        None,
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let policy_arn_type = AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
        None,
        None,
        legacy_validator(|_| Ok(())),
        None,
    );
    let role_schema = ResourceSchema::new("stub.Role")
        .attribute(AttributeSchema::optional("arn", arn_type).build())
        .attribute(AttributeSchema::optional("policy_arn", policy_arn_type).build());

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        target_group_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        vpc_schema.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        role_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    custom_engine(schemas)
}

// Untyped dotted refs participate in both the directory-aware text scan and
// the AST channel. Bare binding refs have no dot for the text scan and use
// only the AST value anchor.
#[test]
fn attributes_block_resolves_untyped_binding_from_sibling_file() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let target_group = test.stub.TargetGroup {}\n",
    )
    .unwrap();
    let attributes = "attributes {\n  target_group_arn = target_group.target_group_arn\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let text_scan_undefined = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Undefined resource: 'target_group'")
        })
        .count();
    let ast_undefined = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Undefined resource 'target_group' in attributes")
        })
        .count();

    assert_eq!(
        text_scan_undefined,
        0,
        "sibling-defined target_group must not be reported as undefined: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(ast_undefined, 0);
    assert!(diagnostics.is_empty());
}

#[test]
fn attributes_block_untyped_unknown_sibling_attribute_uses_core_diagnostic() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes = "attributes {\n  x = tg.no_such\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let unknown_attributes: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'x': unknown attribute 'no_such' on 'tg'")
        })
        .collect();

    assert_eq!(
        unknown_attributes.len(),
        1,
        "the untyped core error must surface exactly once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(unknown_attributes[0].range.start.line, 1);
    assert_eq!(unknown_attributes[0].range.start.character, 2);
    assert_eq!(unknown_attributes[0].range.end.character, 3);
}

#[test]
fn attributes_block_untyped_known_sibling_attribute_has_no_diagnostic() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes = "attributes {\n  x = tg.target_group_arn\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);

    assert!(
        diagnostics.is_empty(),
        "a valid untyped sibling attribute must stay diagnostic-free: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_untyped_param_anchor_respects_name_boundary() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes = "attributes {\n  x_long = tg.target_group_arn\n  x = tg.no_such\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let unknown_attributes: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'x': unknown attribute 'no_such' on 'tg'")
        })
        .collect();

    assert_eq!(unknown_attributes.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(unknown_attributes[0].range.start.line, 2);
    assert_eq!(unknown_attributes[0].range.start.character, 2);
}

#[test]
fn attributes_block_multiline_map_keeps_later_param_anchor_visible() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes =
        "attributes {\n  cfg = {\n    tags = tg.no_such\n  }\n  other: String = tg.also_bad\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let cfg_errors: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'cfg': unknown attribute 'no_such' on 'tg'")
        })
        .collect();
    let other_errors: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'other': unknown attribute 'also_bad' on 'tg'")
        })
        .collect();

    assert_eq!(cfg_errors.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(other_errors.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(diagnostics.len(), 2, "diagnostics: {diagnostics:?}");
    assert_eq!(other_errors[0].range.start.line, 4);
    assert_eq!(other_errors[0].range.start.character, 2);
    assert_eq!(other_errors[0].range.end.line, 4);
    assert_eq!(other_errors[0].range.end.character, 7);
}

#[test]
fn attributes_block_nested_map_entry_does_not_steal_later_param_anchor() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes = "attributes {\n  cfg = {\n    other = 1\n  }\n  other = tg.no_such\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let other_errors: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'other': unknown attribute 'no_such' on 'tg'")
        })
        .collect();

    assert_eq!(other_errors.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(other_errors[0].range.start.line, 4);
    assert_eq!(other_errors[0].range.start.character, 2);
    assert_eq!(other_errors[0].range.end.line, 4);
    assert_eq!(other_errors[0].range.end.character, 7);
}

#[test]
fn attributes_block_untyped_bare_undefined_binding_uses_ast_value_anchor() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    let attributes = "attributes {\n  x = missing\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let ast_undefined: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Undefined resource 'missing' in attributes 'x'")
        })
        .collect();

    assert_eq!(ast_undefined.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:?}");
    assert_eq!(ast_undefined[0].range.start.line, 1);
    assert_eq!(ast_undefined[0].range.start.character, 6);
    assert_eq!(ast_undefined[0].range.end.character, 13);
}

#[test]
fn attributes_block_untyped_dotted_enum_is_not_an_undefined_binding() {
    let engine = test_engine_with_wait_target();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    let attributes =
        "attributes {\n  status = aws.s3.BucketVersioning.VersioningStatus.enabled\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let undefined = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Undefined resource"))
        .count();

    assert_eq!(undefined, 0, "diagnostics: {diagnostics:?}");
}

#[test]
fn attributes_block_resolves_typed_binding_from_sibling_file() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let target_group = test.stub.TargetGroup {}\n",
    )
    .unwrap();
    let attributes =
        "attributes {\n  target_group_arn: String = target_group.target_group_arn\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let undefined = diagnostics.iter().find(|diagnostic| {
        diagnostic.message.contains("Undefined resource")
            && diagnostic.message.contains("target_group")
    });

    assert!(
        undefined.is_none(),
        "sibling-defined target_group must not be reported as undefined: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_still_reports_truly_undefined_sibling_reference() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let target_group = test.stub.TargetGroup {}\n",
    )
    .unwrap();
    let attributes = "attributes {\n  target_group_arn = missing.target_group_arn\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);

    let text_scan_undefined = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Undefined resource: 'missing'"))
        .count();
    let ast_undefined = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Undefined resource 'missing' in attributes")
        })
        .count();

    assert_eq!(
        text_scan_undefined,
        1,
        "a genuinely missing binding must still be reported: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(ast_undefined, 1);
    assert_eq!(diagnostics.len(), 2);
}

#[test]
fn attributes_block_typed_undefined_reference_uses_ast_diagnostic() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(
        base.join("main.crn"),
        "let target_group = test.stub.TargetGroup {}\n",
    )
    .unwrap();
    let attributes = "attributes {\n  x: String = missing.attr\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let typed_undefined = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Undefined resource 'missing' in attributes 'x'")
        })
        .count();

    assert_eq!(
        typed_undefined,
        1,
        "the merged parse must preserve the typed AST undefined-reference diagnostic: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_builtin_string_accepts_sibling_refined_string_like_core() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let vpc = test.stub.Vpc {}\n").unwrap();
    let attributes = "attributes {\n  x: String = vpc.vpc_id\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);

    assert!(
        diagnostics.is_empty(),
        "String versus a refined string attribute must match core validation: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_reports_unknown_attribute_on_sibling_binding() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let tg = test.stub.TargetGroup {}\n").unwrap();
    let attributes = "attributes {\n  x: String = tg.no_such\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let unknown_attributes: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'x': unknown attribute 'no_such' on 'tg'")
        })
        .collect();

    assert_eq!(
        unknown_attributes.len(),
        1,
        "the shared validator must report the sibling attribute typo exactly once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        diagnostics.len(),
        1,
        "the shared validator must be the only attributes-ref existence authority: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(unknown_attributes[0].range.start.line, 1);
    assert_eq!(unknown_attributes[0].range.start.character, 2);
}

#[test]
fn attributes_block_specific_identity_widens_to_simple_annotation() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let role = test.stub.Role {}\n").unwrap();
    let attributes = "attributes {\n  p: Arn = role.policy_arn\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("attribute 'p': type mismatch")
                && diagnostic.message.contains("role.policy_arn")
        })
        .collect();

    assert!(
        mismatches.is_empty(),
        "a specific aws.iam.Policy.Arn source must widen into a bare Arn sink: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_segmented_ref_reports_unknown_struct_field() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let vpc = test.stub.Vpc {}\n").unwrap();
    let attributes = "attributes {\n  x: String = vpc.config.no_such\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let unknown_fields: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'x': unknown field 'no_such' on struct 'VpcConfig'")
        })
        .collect();

    assert_eq!(
        unknown_fields.len(),
        1,
        "segmented refs must use core narrowing and report unknown fields once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(unknown_fields[0].range.start.line, 1);
    assert_eq!(unknown_fields[0].range.start.character, 2);
}

#[test]
fn attributes_block_sibling_custom_type_mismatch_emits_one_diagnostic() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let vpc = test.stub.Vpc {}\n").unwrap();
    let attributes = "attributes {\n  x: SubnetId = vpc.vpc_id\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("type mismatch")
                && diagnostic.message.contains("vpc.vpc_id")
        })
        .count();

    assert_eq!(
        mismatches,
        1,
        "SubnetId versus sibling VpcId must emit exactly one mismatch: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

fn attributes_same_name_in_two_files_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    &'static str,
    &'static str,
) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    let mismatching =
        "let vpc = test.stub.Vpc {}\n\nattributes {\n  region: SubnetId = vpc.vpc_id\n}\n";
    let correct = "attributes {\n  region: String = \"ap-northeast-1\"\n}\n";
    std::fs::write(base.join("a.crn"), mismatching).unwrap();
    std::fs::write(base.join("b.crn"), correct).unwrap();
    (tmp, base, mismatching, correct)
}

#[test]
fn attributes_block_does_not_anchor_sibling_mismatch_in_current_buffer() {
    let engine = attributes_sibling_resource_engine();
    let (_tmp, base, _mismatching, correct) = attributes_same_name_in_two_files_fixture();

    let diagnostics = analyze_with_buffer(&engine, &base, "b.crn", correct);
    let type_mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("type mismatch")
        })
        .collect();

    assert!(
        type_mismatches.is_empty(),
        "the correct region in b.crn must not receive a.crn's mismatch: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn attributes_block_reports_sibling_mismatch_in_its_own_buffer() {
    let engine = attributes_sibling_resource_engine();
    let (_tmp, base, mismatching, _correct) = attributes_same_name_in_two_files_fixture();

    let diagnostics = analyze_with_buffer(&engine, &base, "a.crn", mismatching);
    let type_mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("type mismatch")
                && diagnostic.message.contains("vpc.vpc_id")
        })
        .collect();

    assert_eq!(
        type_mismatches.len(),
        1,
        "a.crn must report its own region mismatch exactly once: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
    assert_eq!(type_mismatches[0].range.start.line, 3);
    assert_eq!(type_mismatches[0].range.start.character, 2);
}

#[test]
fn attributes_block_type_mismatch_resolves_binding_from_sibling_file() {
    let engine = attributes_sibling_resource_engine();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("module");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join("main.crn"), "let vpc = test.stub.Vpc {}\n").unwrap();
    let attributes = "attributes {\n  vpc_id: SubnetId = vpc.vpc_id\n}\n";
    std::fs::write(base.join("attributes.crn"), attributes).unwrap();

    let diagnostics = analyze_with_buffer(&engine, &base, "attributes.crn", attributes);

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .to_ascii_lowercase()
                .contains("type mismatch")
                && diagnostic.message.contains("vpc.vpc_id")
        }),
        "SubnetId versus sibling VpcId must produce a type mismatch: {:?}",
        diagnostics
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>(),
    );
}

/// Build the multi-file fixture used by the #2442 sibling-fn tests.
/// Layout:
///   `<tmp>/helpers.crn`   — `fn double(x: Int) { x }`
///   `<tmp>/providers.crn` — fake `provider test { ... }`
///   `<tmp>/main.crn`      — `<main_text>`
///
/// Returns `(tempdir, base_path)`.
fn sibling_user_fn_fixture(main_text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(base.join("helpers.crn"), "fn double(x: Int) {\n    x\n}\n").unwrap();
    std::fs::write(
        base.join("providers.crn"),
        "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
    )
    .unwrap();
    std::fs::write(base.join("main.crn"), main_text).unwrap();
    (tmp, base)
}

/// A sibling user function is resolved by the directory function prepass, so
/// the isolated buffer's provisional unknown-function error must not leak into
/// the final LSP diagnostics.
#[test]
fn sibling_user_function_is_not_reported_as_unknown() {
    let main_text = "test.r.res {\n    name = \"x\"\n    val = double(2)\n}\n";
    let (_tmp, base) = sibling_user_fn_fixture(main_text);

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let unknown_fn = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function 'double'"));
    assert!(
        unknown_fn.is_none(),
        "sibling-file `fn double(...)` must not produce 'Unknown function' diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// A genuinely unknown name remains a structured parse diagnostic, while the
/// nested sibling function is still recognized by the directory prepass.
#[test]
fn truly_unknown_function_is_reported_without_redlining_sibling_function() {
    let main_text = "test.r.res {\n    name = \"x\"\n    val = triple(double(2))\n}\n";
    let (_tmp, base) = sibling_user_fn_fixture(main_text);

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let unknown_fn = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function 'triple'"));
    assert!(
        unknown_fn.is_some(),
        "genuinely undefined `triple(...)` must still produce 'Unknown function'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let phantom_double = diagnostics
        .iter()
        .find(|d| d.message.contains("Unknown function 'double'"));
    assert!(
        phantom_double.is_none(),
        "sibling-defined `fn double(...)` must NOT be reported even when the directory parse fails. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Build the multi-file fixture used by the #2445 sibling-binding tests.
/// Layout:
///   `<tmp>/network.crn`  — `let bar = awscc.ec2.Vpc { ... }`
///   `<tmp>/providers.crn` — fake `provider awscc { ... }`
///   `<tmp>/main.crn`     — `<main_text>`
///
/// Returns `(tempdir, base_path)`.
fn sibling_resource_binding_fixture(main_text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("network.crn"),
        "let bar = awscc.ec2.Vpc {\n    cidr_block = \"10.0.0.0/16\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        base.join("providers.crn"),
        "provider awscc {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
    )
    .unwrap();
    std::fs::write(base.join("main.crn"), main_text).unwrap();
    (tmp, base)
}

/// Issue #2445: when `let bar = X { ... }` lives in a sibling `.crn`
/// (e.g. `network.crn`) and `attributes { foo = bar.cidr_block }` lives
/// in `main.crn`, the LSP must NOT emit `Undefined resource 'bar'` —
/// even when the directory-merged parse fails for unrelated reasons.
///
/// In the happy path (merged parse succeeds), the existing
/// `BindingNameSet::from_parsed(merged)` already covers sibling
/// bindings. The bug surfaces when `merged = None`: the buffer-only
/// `extract_resource_bindings` fallback misses sibling-defined `bar`
/// and the text-scan `check_undefined_references` redlines it as
/// Unknown.
///
/// CLAUDE.md "Directory-scoped, never single-file": same shape as
/// #2435 / #2442 / #2443 / #2446.
#[test]
fn check_attributes_blocks_skips_sibling_resource_binding() {
    let main_text = "attributes {\n    foo = bar.cidr_block\n}\n";
    let (_tmp, base) = sibling_resource_binding_fixture(main_text);

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let undefined_resource = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("'bar'"));
    assert!(
        undefined_resource.is_none(),
        "sibling-defined `let bar = ...` must not produce 'Undefined resource' diagnostic. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// A bare structural alias is a declaration in its own right. The alias may
/// be declared in a sibling file, and attribute access through it must have
/// the same CLI/LSP scope semantics as access through the original handle.
#[test]
fn sibling_structural_alias_is_known_to_undefined_reference_check() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("network.crn"),
        "let vpc = mock.test.resource { name = \"vpc\" }\nlet network = vpc\n",
    )
    .unwrap();
    std::fs::write(base.join("providers.crn"), "provider mock {}\n").unwrap();
    let main_text = r#"mock.test.resource {
    name = network.name
    parent = network.identifier
}
"#;
    std::fs::write(base.join("main.crn"), main_text).unwrap();

    let diagnostics = analyze_with_buffer(&structural_ref_engine(), &base, "main.crn", main_text);
    let phantom_alias = diagnostics.iter().find(|diagnostic| {
        diagnostic.message.contains("Undefined resource")
            && diagnostic.message.contains("'network'")
    });
    assert!(
        phantom_alias.is_none(),
        "sibling structural alias must come from directory metadata: {diagnostics:#?}",
    );
}

/// The actual user-visible bug from #2445: when the directory parse
/// fails for an unrelated reason (broken sibling), `merged = None`
/// and the buffer-only fallback for `known_bindings` misses sibling-
/// defined `bar`, redlining it as Unknown. Per-file resilient walk
/// must keep sibling let-bindings visible.
#[test]
fn check_attributes_blocks_skips_sibling_resource_binding_even_when_merge_fails() {
    let main_text = "attributes {\n    foo = bar.cidr_block\n}\n";
    let (_tmp, base) = sibling_resource_binding_fixture(main_text);
    // Deliberately broken sibling — half-typed `provider` block. This
    // makes `parse_directory_with_overrides` return Err, so `merged`
    // is None and the LSP must fall back to a per-file walk over
    // siblings to keep `bar` visible.
    std::fs::write(base.join("broken.crn"), "provider gcp { region = ").unwrap();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let undefined_resource = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("'bar'"));
    assert!(
        undefined_resource.is_none(),
        "sibling-defined `bar` must not redline as Unknown when an *unrelated* sibling fails to parse. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Coverage parity: when the merge fails, the fallback path must
/// recognize non-`let`-shaped sibling bindings — `arguments`,
/// `fn`, `import`, etc. — same as the merge-success
/// `BindingNameSet::from_parsed` set. A regression that narrowed the
/// fallback to `let X = ...` headers would phantom-redline arguments
/// referenced from a sibling. Pins the round-2 coverage-parity gap.
#[test]
fn check_attributes_blocks_skips_sibling_argument_even_when_merge_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    // `vpc_id` is declared via an `arguments` block in a sibling — no
    // `let` header. Only the per-file parse + `BindingNameSet`
    // recognizes it as a binding.
    std::fs::write(
        base.join("inputs.crn"),
        "arguments {\n    vpc_id: String\n}\n",
    )
    .unwrap();
    // Force the directory parse to fail so the LSP must hit the
    // (None, Some(base)) fallback arm.
    std::fs::write(base.join("broken.crn"), "provider gcp { region = ").unwrap();
    let main_text = "attributes {\n    foo = vpc_id\n}\n";
    std::fs::write(base.join("main.crn"), main_text).unwrap();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let phantom_arg = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("vpc_id"));
    assert!(
        phantom_arg.is_none(),
        "sibling-defined `arguments {{ vpc_id }}` must not redline as Unknown when an unrelated sibling fails to parse. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Companion to the merge-fails case: even on the directory-aware
/// fallback path, a *truly* unknown identifier must still produce the
/// diagnostic. Pins the regression class where a future refactor
/// returning an empty `known_bindings` set on the `(None, Some)` arm
/// would silently let typos through.
#[test]
fn check_attributes_blocks_warns_when_truly_undefined_even_when_merge_fails() {
    let main_text = "attributes {\n    foo = bar.cidr_block\n    bad = nope.cidr_block\n}\n";
    let (_tmp, base) = sibling_resource_binding_fixture(main_text);
    std::fs::write(base.join("broken.crn"), "provider gcp { region = ").unwrap();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let phantom_bar = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("'bar'"));
    assert!(
        phantom_bar.is_none(),
        "sibling-defined `bar` must not redline. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let undefined_nope = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("nope"));
    assert!(
        undefined_nope.is_some(),
        "truly undefined `nope` must still produce 'Undefined resource' even on the merge-fails fallback. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

/// Companion: a *truly* unknown binding name in `attributes` must
/// still produce the diagnostic.
#[test]
fn check_attributes_blocks_still_warns_when_truly_undefined() {
    // `bar` is sibling-defined; `nope` is not declared anywhere.
    let main_text = "attributes {\n    bad = nope.cidr_block\n}\n";
    let (_tmp, base) = sibling_resource_binding_fixture(main_text);

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main_text, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let undefined_resource = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("nope"));
    assert!(
        undefined_resource.is_some(),
        "genuinely undefined `nope.*` must still produce 'Undefined resource'. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    let phantom_bar = diagnostics
        .iter()
        .find(|d| d.message.contains("Undefined resource") && d.message.contains("'bar'"));
    assert!(
        phantom_bar.is_none(),
        "sibling-defined `bar` must NOT be reported even when another binding is truly unknown. Got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

// =====================================================================
// Empty `${}` interpolation: parser must accept it, LSP surfaces a
// per-location diagnostic, sibling `let` bindings stay visible (#2480).
// =====================================================================

#[test]
fn module_definition_boundary_allows_derived_argument_variable_used_in_sibling_resource() {
    let temp = tempfile::tempdir().expect("create multi-file module fixture");
    std::fs::write(
        temp.path().join("arguments.crn"),
        "arguments {\n  environment: 'dev' | 'prod'\n}\n",
    )
    .expect("write module arguments");
    let source = "let state_bucket_name = \"carina-registry-state-${environment}\"\n";
    let path = temp.path().join("locals.crn");
    std::fs::write(&path, source).expect("write module local");
    std::fs::write(
        temp.path().join("resources.crn"),
        "mock.test.resource {\n  name = state_bucket_name\n}\n",
    )
    .expect("write sibling module resource");

    let diagnostics = test_engine().analyze(
        &create_document_at(source, path.clone()),
        Some(path.as_path()),
    );
    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .contains("unresolved module argument: environment")),
        "the derived local is inlined into and substituted in the sibling resource: {diagnostics:#?}",
    );
}

#[test]
fn module_argument_leftover_is_reported_only_in_its_authored_sibling_file() {
    let temp = tempfile::tempdir().expect("create multi-file module fixture");
    std::fs::write(
        temp.path().join("arguments.crn"),
        "arguments {\n  r: String\n}\n",
    )
    .expect("write module arguments");
    let locals_source = "\n\nlet reg = r\n";
    let locals_path = temp.path().join("locals.crn");
    std::fs::write(&locals_path, locals_source).expect("write module local");
    let providers_source = "provider mock {\n\n  region = reg\n}\n";
    let providers_path = temp.path().join("providers.crn");
    std::fs::write(&providers_path, providers_source).expect("write provider");

    let engine = test_engine();
    let provider_diagnostics = engine.analyze(
        &create_document_at(providers_source, providers_path.clone()),
        Some(providers_path.as_path()),
    );
    assert!(
        provider_diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("unresolved module argument: r")),
        "a sibling-authored module argument must not be anchored in providers.crn: {provider_diagnostics:#?}",
    );

    let locals_diagnostics = engine.analyze(
        &create_document_at(locals_source, locals_path.clone()),
        Some(locals_path.as_path()),
    );
    let authored = locals_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("unresolved module argument: r"))
        .unwrap_or_else(|| {
            panic!("the authored sibling must retain its diagnostic: {locals_diagnostics:#?}")
        });
    assert_eq!(authored.range.start.line, 2, "{authored:#?}");
    assert_eq!(authored.range.start.character, 10, "{authored:#?}");
    assert_eq!(authored.range.end.line, 2, "{authored:#?}");
    assert_eq!(authored.range.end.character, 11, "{authored:#?}");
}

#[test]
fn spaced_interpolation_expression_has_no_error_diagnostic() {
    let engine = test_engine();
    let buffer = "let value = \"x-${join('-', ['a'])}\"\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != Some(DiagnosticSeverity::ERROR)),
        "valid whitespace inside interpolation must not produce an error: {diagnostics:#?}"
    );
}

#[test]
fn multiline_interpolation_expression_has_no_spurious_diagnostic() {
    let engine = test_engine();
    let buffer = "let value = \"x-${\n  join('-', ['a']) // trailing comment\n}\"\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);

    assert!(
        diagnostics.iter().all(|diagnostic| {
            diagnostic.severity != Some(DiagnosticSeverity::ERROR)
                && !diagnostic
                    .message
                    .to_lowercase()
                    .contains("empty interpolation")
        }),
        "normal trivia at non-empty interpolation edges must not produce an error or empty hint: {diagnostics:#?}"
    );
}

#[test]
fn empty_interpolation_after_multiline_interpolation_still_errors() {
    let engine = test_engine();
    let buffer = "let x = 'a'\nlet value = \"a-${\n  x\n} ${}\"\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);

    let empty_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .collect();
    assert_eq!(
        empty_diagnostics.len(),
        1,
        "the same string's trailing `${{}}` must still produce an error: {diagnostics:#?}"
    );
    assert_eq!(empty_diagnostics[0].range.start.line, 3);
    assert_eq!(
        empty_diagnostics[0].severity,
        Some(DiagnosticSeverity::ERROR)
    );
}

#[test]
fn interpolation_like_text_inside_line_comment_does_not_warn() {
    let engine = test_engine();
    let buffer = "let x = 'a'\nlet value = \"${ x // ${}\n}\"\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);

    assert!(
        diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .to_lowercase()
            .contains("empty interpolation")),
        "`${{}}` inside an interpolation line comment is not an empty interpolation: {diagnostics:#?}"
    );
}

#[test]
fn inline_whitespace_only_interpolations_emit_errors() {
    let engine = test_engine();
    let buffer = "let value = \"${} / ${ }\"\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);

    let empty_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .collect();
    assert_eq!(
        empty_diagnostics.len(),
        2,
        "both `${{}}` and `${{ }}` must produce an empty-interpolation error: {diagnostics:#?}"
    );
    assert!(
        empty_diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
        "both empty-interpolation diagnostics must be errors: {empty_diagnostics:#?}"
    );
}

#[test]
fn empty_interpolation_after_heredoc_body_punctuation_still_errors() {
    let engine = test_engine();

    for body in ["it's", "rm -rf /tmp/*"] {
        let buffer = format!("let note = <<EOT\n{body}\nEOT\nlet value = \"${{}}\"\n");
        let diagnostics = engine.analyze(&create_document(&buffer), None);
        let empty_count = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .message
                    .to_lowercase()
                    .contains("empty interpolation")
            })
            .count();

        assert_eq!(
            empty_count, 1,
            "heredoc body {body:?} must not hide the following empty interpolation: {diagnostics:#?}"
        );
    }
}

#[test]
fn backslash_prefixed_empty_interpolation_in_heredoc_errors() {
    let engine = test_engine();
    let buffer = "let value = <<EOT\n\\${}\nEOT\n";
    let diagnostics = engine.analyze(&create_document(buffer), None);
    let empty = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .unwrap_or_else(|| panic!("`\\${{}}` in an unquoted heredoc must error: {diagnostics:#?}"));

    assert_eq!(empty.range.start.line, 1);
    assert_eq!(empty.range.start.character, 1);
    assert_eq!(empty.range.end.character, 4);
}

#[test]
fn empty_interpolation_emits_diagnostic_at_span() {
    let engine = test_engine();
    let buffer = "let bucket = aws.s3.Bucket {\n    name = \"arn:${}:root\"\n}\n";
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let empty = diagnostics
        .iter()
        .find(|d| d.message.to_lowercase().contains("empty interpolation"))
        .unwrap_or_else(|| {
            panic!(
                "expected an EmptyInterpolation diagnostic; got: {:?}",
                diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });

    assert_eq!(
        empty.message,
        "aws.s3.Bucket.bucket: attribute `name`: empty interpolation `${}` — fill in the expression or remove it",
        "the diagnostic range already identifies the source location",
    );

    // The `${}` span sits inside `name = "arn:${}:root"` on line 1.
    // The diagnostic must point at that interpolation, not the whole
    // string and not the resource block.
    assert_eq!(
        empty.range.start.line, 1,
        "expected line 1, got: {:?}",
        empty
    );
    let line1 = buffer.lines().nth(1).unwrap();
    let dollar_col = line1.find("${").unwrap() as u32;
    let close_col = (line1.find("${}").unwrap() + "${}".len()) as u32;
    assert_eq!(
        empty.range.start.character, dollar_col,
        "diagnostic start should be at `${{`; got: {:?}",
        empty.range
    );
    assert_eq!(
        empty.range.end.character, close_col,
        "diagnostic end should be just after `}}`; got: {:?}",
        empty.range
    );
}

#[test]
fn empty_interpolations_use_their_authored_locations_not_collection_order() {
    let buffer = "let a = \"x${}\"\nmock.test.resource { name = \"${}\" }\n";
    let diagnostics = test_engine().analyze(&create_document(buffer), None);
    let empty_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .collect::<Vec<_>>();

    let local = empty_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("let `a`"))
        .unwrap_or_else(|| panic!("missing located `let a` diagnostic: {diagnostics:#?}"));
    let resource = empty_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("attribute `name`"))
        .unwrap_or_else(|| panic!("missing located resource diagnostic: {diagnostics:#?}"));

    assert_eq!(local.range.start.line, 0, "{local:#?}");
    assert_eq!(resource.range.start.line, 1, "{resource:#?}");
    assert_eq!(local.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(resource.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn inlined_empty_interpolation_keeps_one_authored_span_for_every_leftover() {
    let buffer = "let a = \"x${}\"\nmock.test.resource { name = a }\n";
    let diagnostics = test_engine().analyze(&create_document(buffer), None);
    let empty_diagnostics = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .collect::<Vec<_>>();
    let expected_start = buffer.lines().next().unwrap().find("${}").unwrap() as u32;
    let expected_end = expected_start + 3;

    assert_eq!(empty_diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(
        empty_diagnostics.iter().all(|diagnostic| {
            diagnostic.range.start.line == 0
                && diagnostic.range.start.character == expected_start
                && diagnostic.range.end.line == 0
                && diagnostic.range.end.character == expected_end
                && diagnostic.severity == Some(DiagnosticSeverity::ERROR)
        }),
        "both collected leftovers must retain the authored interpolation span: {empty_diagnostics:#?}",
    );
}

#[test]
fn sibling_empty_interpolation_is_not_misattributed_to_current_buffer() {
    let temp = tempfile::tempdir().expect("create multi-file fixture");
    let sibling_source = "let broken = \"${}\"\n";
    let sibling_path = temp.path().join("locals.crn");
    std::fs::write(&sibling_path, sibling_source).expect("write sibling local");
    let current_source = "mock.test.resource { name = broken }\n";
    let current_path = temp.path().join("main.crn");
    std::fs::write(&current_path, current_source).expect("write current resource");

    let engine = test_engine();
    let current_diagnostics = engine.analyze(
        &create_document_at(current_source, current_path.clone()),
        Some(current_path.as_path()),
    );
    assert!(
        current_diagnostics.iter().all(|diagnostic| !diagnostic
            .message
            .to_lowercase()
            .contains("empty interpolation")),
        "a sibling-authored interpolation must not be anchored in the current buffer: {current_diagnostics:#?}",
    );

    let sibling_diagnostics = engine.analyze(
        &create_document_at(sibling_source, sibling_path.clone()),
        Some(sibling_path.as_path()),
    );
    let sibling_empty = sibling_diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .to_lowercase()
                .contains("empty interpolation")
        })
        .unwrap_or_else(|| {
            panic!("the authored sibling must retain its own diagnostic: {sibling_diagnostics:#?}")
        });
    assert_eq!(sibling_empty.range.start.line, 0);
    assert_eq!(sibling_empty.severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn empty_interpolation_does_not_cascade_undefined_for_sibling_bindings() {
    // Cross-file repro from #2480: an empty `${}` in main.crn must not
    // cause sibling-declared `bucket` (in backend.crn) to be flagged
    // as undefined.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    std::fs::write(
        base.join("backend.crn"),
        "let bucket = aws.s3.Bucket {\n    name = 'state-bucket'\n}\n",
    )
    .unwrap();
    let main = "aws.s3.BucketPolicy {\n    bucket = bucket.bucket_name\n    policy = \"arn:${}:root\"\n}\n";
    std::fs::write(base.join("main.crn"), main).unwrap();

    let engine = test_engine();
    let path = base.join("main.crn");
    let doc = create_document_at(main, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));

    let bucket_undefined = diagnostics
        .iter()
        .any(|d| d.message.contains("Undefined") && d.message.contains("bucket"));
    assert!(
        !bucket_undefined,
        "sibling `let bucket = ...` must stay visible despite empty `${{}}`; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn empty_interpolation_does_not_misfire_on_nested_object_literal() {
    // `${ {a = 1}.a }` is a non-empty expression that *contains* `{}`
    // within. The brace-balanced scanner must stop at the matching `}`,
    // not the first one, so this must NOT produce an EmptyInterpolation
    // diagnostic.
    let engine = test_engine();
    let buffer = "let x = aws.s3.Bucket {\n    name = \"prefix-${ {a = 1}.a }\"\n}\n";
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let empty = diagnostics
        .iter()
        .find(|d| d.message.to_lowercase().contains("empty interpolation"));
    assert!(
        empty.is_none(),
        "nested object literal inside `${{}}` is not empty; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn empty_interpolation_does_not_misfire_on_escape_inside_braces() {
    // `${\\}` contains an escaped backslash inside the braces. The body
    // is non-whitespace (a `\\` pair), so the scanner must NOT flag it
    // as empty.
    let engine = test_engine();
    let buffer = "let x = aws.s3.Bucket {\n    name = \"prefix-${\\\\}\"\n}\n";
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let empty = diagnostics
        .iter()
        .find(|d| d.message.to_lowercase().contains("empty interpolation"));
    assert!(
        empty.is_none(),
        "escape pair inside `${{}}` is not empty; got: {:?}",
        diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}

#[test]
fn empty_interpolation_diagnostic_is_error() {
    let engine = test_engine();
    let buffer = "let x = aws.s3.Bucket {\n    name = \"prefix-${}\"\n}\n";
    let doc = create_document(buffer);
    let diagnostics = engine.analyze(&doc, None);

    let empty = diagnostics
        .iter()
        .find(|d| d.message.to_lowercase().contains("empty interpolation"))
        .expect("expected an EmptyInterpolation diagnostic");
    assert_eq!(
        empty.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "the LSP must agree with the CLI expansion-boundary error"
    );
}

#[test]
fn lsp_flags_undefined_let_reference_in_provider_block() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let file = base.join("providers.crn");
    std::fs::write(
        &file,
        r#"
provider awscc {
  source       = "github.com/carina-rs/carina-provider-awscc"
  revision     = "main"
  default_tags = nonexistent.tags
}
"#,
    )
    .unwrap();

    let engine = test_engine();
    let buffer = std::fs::read_to_string(&file).unwrap();
    let diags = analyze_with_buffer(&engine, base, "providers.crn", &buffer);

    assert!(
        diags.iter().any(|d| d.message.contains("nonexistent")),
        "LSP must surface UndefinedIdentifier for `nonexistent`; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn directory_bare_undefined_deferred_iterable_is_reported_exactly_once() {
    let fixture = tempfile::tempdir().expect("undefined iterable LSP fixture");
    let source = "let items = ['one']\n\
                  let resources = for item in itmes {\n\
                    mock.test.resource { name = item }\n\
                  }\n";
    let file = fixture.path().join("main.crn");
    std::fs::write(&file, source).expect("write LSP fixture");

    let diagnostics = analyze_with_buffer(&test_engine(), fixture.path(), "main.crn", source);
    let undefined = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Undefined identifier `itmes`"))
        .collect::<Vec<_>>();

    assert_eq!(
        undefined.len(),
        1,
        "the typed undefined iterable must have one LSP owner: {diagnostics:#?}",
    );
}

#[test]
fn lsp_flags_non_map_default_tags() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let file = base.join("providers.crn");
    std::fs::write(
        &file,
        r#"
let bad = "not a map"

provider awscc {
  source       = "github.com/carina-rs/carina-provider-awscc"
  revision     = "main"
  default_tags = bad
}
"#,
    )
    .unwrap();

    let engine = test_engine();
    let buffer = std::fs::read_to_string(&file).unwrap();
    let diags = analyze_with_buffer(&engine, base, "providers.crn", &buffer);

    assert!(
        diags.iter().any(|d| d.message.contains("default_tags")),
        "LSP must surface non-map default_tags error; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn provider_finalize_keeps_independent_diagnostic_after_module_witness_failure() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(module.join("main.crn"), "arguments { count: Int }\n").unwrap();
    let main = r#"let module = use { source = './module' }
let instance = module {
  count = 'wrong'
}
"#;
    let providers = r#"let bad = 'not a map'
provider awscc {
  default_tags = bad
}
"#;
    std::fs::write(root.path().join("main.crn"), main).unwrap();
    std::fs::write(root.path().join("providers.crn"), providers).unwrap();

    let engine = test_engine();
    let doc = create_document(providers);
    let finalize_error = engine
        .finalize_provider_diagnostic(&doc, Some("providers.crn"), root.path())
        .expect("independent provider finalization must survive a module witness failure");
    assert!(finalize_error.to_string().contains("default_tags"));

    let diagnostics = analyze_with_buffer(&engine, root.path(), "providers.crn", providers);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("default_tags")),
        "an independent provider error must not be hidden by failed expansion: {diagnostics:#?}"
    );
}

#[test]
fn provider_finalize_suppresses_only_value_dependent_on_failed_module() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments { count: Int }\nattributes { tags: map(String) = { env = 'dev' } }\n",
    )
    .unwrap();
    let main = r#"let module = use { source = './module' }
let instance = module { count = 'wrong' }
"#;
    let providers = r#"provider awscc {
  default_tags = instance.tags
}
"#;
    std::fs::write(root.path().join("main.crn"), main).unwrap();
    std::fs::write(root.path().join("providers.crn"), providers).unwrap();

    let engine = test_engine();
    let doc = create_document(providers);
    assert!(
        engine
            .finalize_provider_diagnostic(&doc, Some("providers.crn"), root.path())
            .is_none(),
        "a provider value downstream of failed expansion cannot be finalized independently"
    );
    let diagnostics = analyze_with_buffer(&engine, root.path(), "providers.crn", providers);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("default_tags")),
        "the dependent provider value must not create a misleading shape error: {diagnostics:#?}"
    );
}

#[test]
fn lsp_flags_wait_unknown_target() {
    let engine = test_engine_with_wait_target();
    let src = r#"
let cert = aws.acm.Certificate {
    domain_name = "example.com"
    status = "PENDING"
}
let waited = wait nonexistent {
    until = nonexistent.status == "ISSUED"
}
"#;
    let doc = create_document(src);
    let diags = engine.analyze(&doc, None);
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("nonexistent") && d.message.contains("not a known")),
        "LSP must surface unknown wait target; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn lsp_rejects_sibling_data_source_wait_target_at_target_anchor() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("providers.crn"), "provider aws {}\n").unwrap();
    std::fs::write(
        fixture.path().join("data.crn"),
        r#"let lookup = read aws.acm.Certificate {
    domain_name = "example.com"
}
"#,
    )
    .unwrap();
    let source = r#"let found = wait lookup {
    until = lookup.status == "ISSUED"
}
"#;
    let path = fixture.path().join("main.crn");
    let diagnostics = test_engine_with_wait_target()
        .analyze(&create_document_at(source, path.clone()), Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.message
                == "wait `found`: target binding `lookup` is a data source; wait targets must be managed resources"
        })
        .unwrap_or_else(|| panic!("missing data-source wait diagnostic: {diagnostics:#?}"));

    assert_eq!(diagnostic.range.start.line, 0);
    assert_eq!(diagnostic.range.start.character, 17);
    assert_eq!(diagnostic.range.end.line, 0);
    assert_eq!(diagnostic.range.end.character, 23);
}

#[test]
fn lsp_flags_wait_unknown_attribute() {
    let engine = test_engine_with_wait_target();
    let src = r#"
let cert = aws.acm.Certificate {
    domain_name = "example.com"
    status = "PENDING"
}
let waited = wait cert {
    until = cert.statu == "ISSUED"
}
"#;
    let doc = create_document(src);
    let diags = engine.analyze(&doc, None);
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("statu") && d.message.contains("unknown attribute")),
        "LSP must surface unknown wait until attribute; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn lsp_requires_quotes_for_unresolved_wait_enum_identifiers() {
    let engine = test_engine_with_wait_target();
    let source = r#"
let cert = aws.acm.Certificate {
    domain_name = "example.com"
    status = "PENDING"
}
let state_ready = wait cert {
    until = cert.status == ready
}
let enum_ready = wait cert {
    until = cert.enum_status == aws.acm.Certificate.Status.bogus
}
"#;
    let document = create_document(source);
    let diagnostics = engine.analyze(&document, None);
    let messages = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(messages.contains(&"wait `state_ready`: cannot resolve `ready` as an enum value of `cert.status`; quote it to compare as a string"));
    assert!(messages.contains(&"wait `enum_ready`: cannot resolve `aws.acm.Certificate.Status.bogus` as an enum value of `cert.enum_status`; quote it to compare as a string"));

    let ready = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("cannot resolve `ready`"))
        .expect("ready diagnostic");
    assert_eq!(ready.range.start.line, 6);
    assert_eq!(ready.range.start.character, 27);
    assert_eq!(ready.range.end.line, 6);
    assert_eq!(ready.range.end.character, 32);
}

#[test]
fn lsp_accepts_valid_wait_block() {
    let engine = test_engine_with_wait_target();
    let src = r#"
let cert = aws.acm.Certificate {
    domain_name = "example.com"
    status = "PENDING"
}
let waited = wait cert {
    until = cert.status == "ISSUED"
}
"#;
    let doc = create_document(src);
    let diags = engine.analyze(&doc, None);
    assert!(
        !diags.iter().any(|d| d.message.contains("wait")),
        "valid wait block should produce no wait-specific diagnostics; got: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn lsp_accepts_block_syntax_inside_ref_typed_attribute() {
    // Regression for carina#3349: the LSP per-keystroke diagnostic
    // pass previously matched on `&attr_schema.attr_type` raw, so a
    // `Ref("LifecycleConfiguration")` attribute fell through every
    // arm and silently produced no diagnostics — including no
    // "Required attribute 'rules' is missing" — *but the apply path
    // through `resolve_block_names` did surface that error*. The fix
    // peels `Ref` against `schema.defs` at every shape gate the LSP
    // touches. Pin "valid block syntax inside a Ref-typed attribute
    // yields no schema-shape diagnostics" so a future refactor that
    // drops the peel cannot silently regress.
    let engine = test_engine_with_ref_lifecycle_like_schema();
    let src = r#"
let bucket = awscc.s3.Bucket {
    lifecycle_configuration = {
        rule {
            id = "expire-old-objects"
        }
        rule {
            id = "transition-to-glacier"
        }
    }
}
"#;
    let doc = create_document(src);
    let diags = engine.analyze(&doc, None);
    let block_msgs: Vec<_> = diags
        .iter()
        .filter(|d| {
            d.message.contains("Required attribute 'rules'")
                || d.message.contains("cannot use block syntax")
                || d.message.contains("Type mismatch")
        })
        .map(|d| d.message.clone())
        .collect();
    assert!(
        block_msgs.is_empty(),
        "Ref-typed attribute should not surface block-syntax / required / type-mismatch diagnostics for the documented `rule {{ }}` shape; got: {:?}",
        block_msgs,
    );
}

#[test]
fn lsp_surfaces_missing_required_field_inside_ref_typed_attribute() {
    // Positive-assertion regression for carina#3349. Unlike the
    // sibling test above (which asserts the *absence* of false
    // positives — passes even if the LSP peel is deleted because
    // dropping `Ref` silently disables struct validation), this test
    // asserts that struct-field validation actually descends through
    // the `Ref` and reports a missing required field. Without the
    // peel calls in `carina-lsp/src/diagnostics/mod.rs`, the
    // `is_struct_shape` gate returns false for the Ref-typed
    // attribute and the validator never visits the nested `rule { }`
    // block — so a future refactor that drops the peel would fail
    // this test outright. The `Rule.id` field is declared `.required()`
    // in `test_engine_with_ref_lifecycle_like_schema` for this
    // purpose.
    let engine = test_engine_with_ref_lifecycle_like_schema();
    let src = r#"
let bucket = awscc.s3.Bucket {
    lifecycle_configuration = {
        rule {
        }
    }
}
"#;
    let doc = create_document(src);
    let diags = engine.analyze(&doc, None);
    let missing_id: Vec<_> = diags
        .iter()
        .filter(|d| {
            (d.message.contains("Required") || d.message.contains("required"))
                && d.message.contains("id")
        })
        .map(|d| d.message.clone())
        .collect();
    assert!(
        !missing_id.is_empty(),
        "LSP must surface 'required field id is missing' through a Ref-typed attribute. \
         If this test fails, the Ref-peel in carina-lsp/src/diagnostics/mod.rs has \
         regressed (same class as carina#3349). All diagnostics: {:?}",
        diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

// =====================================================================
// Directory-wide duplicate declarations (carina#3714).
// =====================================================================

#[test]
fn duplicate_provider_kind_across_buffer_and_sibling_is_one_anchored_error() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    std::fs::write(
        base.join("providers.crn"),
        "provider mock {\n  source = 'file:///mock.wasm'\n}\n",
    )
    .unwrap();
    let source = "provider mock {}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .starts_with("duplicate provider kind 'mock'")
        })
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        duplicates[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert_eq!(duplicates[0].range.start.line, 0);
    assert_eq!(duplicates[0].range.start.character, 9);
    assert_eq!(duplicates[0].range.end.character, 13);
    assert!(duplicates[0].message.contains("main.crn"));
    assert!(duplicates[0].message.contains("providers.crn"));
}

#[test]
fn duplicate_export_across_buffer_and_sibling_is_one_anchored_error() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    std::fs::write(
        base.join("sibling.crn"),
        "exports {\n  x = \"sibling\"\n}\n",
    )
    .unwrap();
    let source = "exports {\n  x = \"buffer\"\n}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("duplicate export 'x'"))
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        duplicates[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert_eq!(duplicates[0].range.start.line, 1);
    assert_eq!(duplicates[0].range.start.character, 2);
    assert!(duplicates[0].message.contains("main.crn"));
    assert!(duplicates[0].message.contains("sibling.crn"));
}

#[test]
fn duplicate_binding_across_buffer_and_sibling_is_one_anchored_error() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    std::fs::write(base.join("sibling.crn"), "let v = \"sibling\"\n").unwrap();
    let source = "let v = \"buffer\"\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("duplicate binding 'v'"))
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        duplicates[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert_eq!(duplicates[0].range.start.line, 0);
    assert_eq!(duplicates[0].range.start.character, 4);
    assert!(duplicates[0].message.contains("main.crn"));
    assert!(duplicates[0].message.contains("sibling.crn"));
}

#[test]
fn duplicate_user_function_anchors_on_exact_identifier() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    std::fs::write(base.join("sibling.crn"), "fn tag() {\n  \"sibling\"\n}\n").unwrap();
    let source = "fn tag_name() {\n  \"longer name\"\n}\n\nfn tag() {\n  \"buffer\"\n}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("duplicate function 'tag'"))
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(duplicates[0].range.start.line, 4);
    assert_eq!(duplicates[0].range.start.character, 3);
    assert_eq!(duplicates[0].range.end.line, 4);
    assert_eq!(duplicates[0].range.end.character, 6);
}

#[test]
fn same_block_duplicate_export_has_error_without_last_wins_warning() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let source = "exports {\n  x = \"one\"\n  x = \"two\"\n}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("duplicate export 'x'"))
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        duplicates[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("last value")),
        "export duplicates must not retain a last-write-wins warning: {diagnostics:#?}",
    );
}

#[test]
fn same_block_duplicate_module_attribute_has_error_without_last_wins_warning() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    let source = "attributes {\n  x = \"one\"\n  x = \"two\"\n}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .starts_with("duplicate module attribute 'x'")
        })
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        duplicates[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("last value")),
        "module attribute duplicates must not retain a last-write-wins warning: {diagnostics:#?}",
    );
}

#[test]
fn same_block_duplicate_export_survives_broken_sibling_parse() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    std::fs::write(base.join("broken.crn"), "provider gcp { region = ").unwrap();
    let source = "exports {\n  x = \"one\"\n  x = \"two\"\n}\n";
    let path = base.join("main.crn");
    let doc = create_document_at(source, path.clone());

    let diagnostics = test_engine().analyze(&doc, Some(&path));
    let duplicates: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("duplicate export 'x'"))
        .collect();

    assert_eq!(duplicates.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(duplicates[0].range.start.line, 1);
    assert_eq!(duplicates[0].range.start.character, 2);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("last value")),
        "the fallback must not restore the last-write-wins warning: {diagnostics:#?}",
    );
}

// =====================================================================
// Directory-wide tag-key style diagnostics (carina#3718).
// =====================================================================

fn write_tag_key_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    for (relative_path, source) in files {
        let path = temp.path().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, source).unwrap();
    }
    temp
}

#[test]
fn environment_plaintext_is_redacted_across_lsp_directory_typecheck_paths() {
    type DirectoryCase<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);

    let cases: &[DirectoryCase<'_>] = &[
        (
            "module literal union argument",
            "main.crn",
            &[
                (
                    "main.crn",
                    "let m = use { source = './m' }\nm {\n  n = f()\n}\n",
                ),
                ("functions.crn", "fn f() { env('LEAKX') }\n"),
                ("m/main.crn", "arguments { n: 'dev' | 'prod' }\n"),
            ],
        ),
        (
            "module list argument",
            "main.crn",
            &[
                (
                    "main.crn",
                    "let m = use { source = './m' }\nm {\n  l = list_values()\n}\n",
                ),
                (
                    "functions.crn",
                    "fn list_values() { [env('LEAKX'), 'b'] }\n",
                ),
                ("m/main.crn", "arguments { l: list(Int) }\n"),
            ],
        ),
        (
            "literal union export",
            "exports.crn",
            &[
                ("functions.crn", "fn f() { env('LEAKX') }\n"),
                ("exports.crn", "exports {\n  e: 'dev' | 'prod' = f()\n}\n"),
            ],
        ),
        (
            "nested function export",
            "exports.crn",
            &[
                ("functions.crn", "fn k(x) { x }\n"),
                ("exports.crn", "exports {\n  q: Int = k(env('LEAKX'))\n}\n"),
            ],
        ),
    ];

    unsafe {
        std::env::set_var("LEAKX", "hunter2SECRET");
    }
    for (label, filename, files) in cases {
        let temp = write_tag_key_fixture(files);
        let buffer = std::fs::read_to_string(temp.path().join(filename)).unwrap();
        let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), filename, &buffer);

        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("got string")),
            "{label}: diagnostics={diagnostics:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("hunter2SECRET")),
            "{label} leaked env plaintext: diagnostics={diagnostics:#?}"
        );
    }
    unsafe {
        std::env::remove_var("LEAKX");
    }
}

#[test]
fn secret_valued_exports_are_rejected_for_direct_derived_and_nested_shapes() {
    let source = r#"
let s = secret("topsecretXYZ")

exports {
  direct: String = s
  builtin: String = upper(s)
  interpolated: String = "x-${s}"
  inline_interpolated = "x-${secret("topsecretXYZ")}"
  decrypted: String = decrypt("ciphertext-marker")
  nested_list: list(String) = [s]
  nested_map: map(String) = { key = s }
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", source)]);

    let mut engine = test_engine();
    engine
        .provider_context
        .set_decryptor_for_test(Box::new(|_, _| Ok("decrypted-marker".to_string())));
    let diagnostics = analyze_with_buffer(&engine, temp.path(), "main.crn", source);

    for name in [
        "direct",
        "builtin",
        "interpolated",
        "inline_interpolated",
        "decrypted",
        "nested_list",
        "nested_map",
    ] {
        let expected = format!("export '{name}' cannot hold a secret value");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "missing {expected:?}: {diagnostics:#?}",
        );
    }
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("topsecretXYZ")),
        "secret leaked: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("decrypted-marker")),
        "decrypted secret leaked: {diagnostics:#?}",
    );
}

#[test]
fn secret_exports_through_resource_and_module_attributes_are_rejected_across_files() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);

    type Case<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);
    let cases: &[Case<'_>] = &[
        (
            "resource attribute",
            "exports.crn",
            "c",
            &[
                (
                    "main.crn",
                    r#"let r = mock.test.resource {
  name = "alpha"
  comment = secret("hunter2SECRET")
}
"#,
                ),
                ("exports.crn", "exports {\n  c: String = r.comment\n}\n"),
            ],
        ),
        (
            "module attribute",
            "exports.crn",
            "e",
            &[
                (
                    "main.crn",
                    r#"let m = use { source = "./modules/pass" }
let inst = m { pw = secret("hunter2SECRET") }
"#,
                ),
                ("exports.crn", "exports {\n  e: String = inst.out\n}\n"),
                (
                    "modules/pass/main.crn",
                    "arguments { pw: String }\nattributes { out: String = pw }\n",
                ),
            ],
        ),
        (
            "derived module attribute",
            "exports.crn",
            "x",
            &[
                (
                    "providers.crn",
                    "let r = mock.test.resource { name = 'alpha' }\n",
                ),
                (
                    "main.crn",
                    r#"let m = use { source = "./modules/upper" }
let inst = m { pw = "${r.identifier}${secret("hunter2SECRET")}" }
"#,
                ),
                ("exports.crn", "exports {\n  x: String = inst.out\n}\n"),
                (
                    "modules/upper/main.crn",
                    "arguments { pw: String }\nattributes { out: String = upper(pw) }\n",
                ),
            ],
        ),
    ];

    for (label, filename, export_name, files) in cases {
        let temp = write_tag_key_fixture(files);
        let source = std::fs::read_to_string(temp.path().join(filename)).unwrap();
        let diagnostics = analyze_with_buffer(&engine, temp.path(), filename, &source);
        let expected = format!("export '{export_name}' cannot hold a secret value");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message == expected),
            "{label} missing {expected:?}: {diagnostics:#?}",
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("hunter2SECRET")),
            "{label} leaked plaintext: {diagnostics:#?}",
        );
    }
}

#[test]
fn secret_export_does_not_hide_sibling_export_type_error() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = r#"let r = mock.test.resource {
  name = "alpha"
  comment = secret("hunter2SECRET")
}

exports {
  c: String = r.comment
  n: Int = "notint"
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", source)]);
    let diagnostics = analyze_with_buffer(&engine, temp.path(), "main.crn", source);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "export 'c' cannot hold a secret value"),
        "missing secret export diagnostic: {diagnostics:#?}",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got string 'notint'")
        }),
        "missing sibling type diagnostic: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("hunter2SECRET")),
        "secret leaked: {diagnostics:#?}",
    );
}

#[test]
fn one_line_module_call_type_mismatch_is_anchored_to_argument_value() {
    let main = "let m = use { source = './m' }\n\
let inst = m { n = 'abc' label = 'x' }\n";
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        ("m/main.crn", "arguments {\n  n: Int\n  label: String\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("expected Int, got string 'abc'")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(1, 19), Position::new(1, 24))
    );
}

#[test]
fn one_line_module_call_unknown_argument_is_anchored_to_argument_key() {
    let main = "let m = use { source = './m' }\n\
let inst = m { n = 1 label = 'x' nn = 2 }\n";
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        ("m/main.crn", "arguments {\n  n: Int\n  label: String\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Unknown argument 'nn'"))
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(1, 33), Position::new(1, 35))
    );
}

#[test]
fn one_line_module_call_missing_argument_is_anchored_to_second_call() {
    let main = "let m = use { source = './m' }\n\
let first = m { n = 1 label = 'x' }\n\
let second = m { n = 2 }\n";
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        ("m/main.crn", "arguments {\n  n: Int\n  label: String\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Missing required argument 'label'")
        })
        .collect::<Vec<_>>();

    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(2, 13), Position::new(2, 24))
    );
}

#[test]
fn issue_3859_anonymous_one_line_calls_keep_their_own_ranges() {
    let main = "let m = use { source = './m' }\n\
m { n = 'abc' }\n\
m { label = 'x' }\n";
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("m/main.crn", "arguments {\n  n: Int\n  label: String\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let mismatch = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("expected Int, got string 'abc'")
        })
        .collect::<Vec<_>>();
    let missing_label = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Missing required argument 'label'")
        })
        .collect::<Vec<_>>();

    assert_eq!(mismatch.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        mismatch[0].range,
        Range::new(Position::new(1, 8), Position::new(1, 13))
    );
    assert_eq!(missing_label.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        missing_label[0].range,
        Range::new(Position::new(1, 0), Position::new(1, 15))
    );
    assert_ne!(missing_label[0].range.start.line, 0);
}

#[test]
fn issue_3859_heredoc_inside_call_keeps_unknown_parameter_range() {
    let main = "let m = use { source = './m' }\n";
    let calls = r#"let inst = m {
  doc = <<EOT
a
b
EOT
  nn = 1
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("calls.crn", calls),
        ("m/main.crn", "arguments { doc: String }\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "calls.crn", calls);
    let unknown = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Unknown argument 'nn'"))
        .collect::<Vec<_>>();

    assert_eq!(unknown.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        unknown[0].range,
        Range::new(Position::new(5, 2), Position::new(5, 4))
    );
}

#[test]
fn issue_3859_module_call_diagnostics_stay_in_the_owning_sibling_file() {
    let main = "let m = use { source = './m' }\n";
    let calls = "\n\nlet inst = m { nn = 1 }\n";
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("calls.crn", calls),
        ("m/main.crn", "arguments { n: Int }\n"),
    ]);

    let main_diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    assert!(
        main_diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("Unknown argument 'nn'")),
        "main.crn diagnostics: {main_diagnostics:#?}"
    );

    let call_diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "calls.crn", calls);
    let unknown = call_diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Unknown argument 'nn'"))
        .collect::<Vec<_>>();
    assert_eq!(unknown.len(), 1, "diagnostics: {call_diagnostics:#?}");
    assert_eq!(
        unknown[0].range,
        Range::new(Position::new(2, 15), Position::new(2, 17))
    );
}

#[test]
fn module_call_diagnostics_use_original_line_after_heredoc() {
    let main = r#"let m = use { source = './m' }
let resource = mock.test.resource {
  name = <<EOT
alpha
beta
gamma
delta
EOT
}
let inst = m { nn = 1 }
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        ("m/main.crn", "arguments {\n  label: String\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let unknown = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("Unknown argument 'nn'"))
        .unwrap_or_else(|| panic!("missing unknown-parameter diagnostic: {diagnostics:#?}"));
    let missing = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("Missing required argument 'label'")
        })
        .unwrap_or_else(|| panic!("missing required-parameter diagnostic: {diagnostics:#?}"));

    assert_eq!(
        unknown.range,
        Range::new(Position::new(9, 15), Position::new(9, 17))
    );
    assert_eq!(
        missing.range,
        Range::new(Position::new(9, 11), Position::new(9, 23))
    );
}

#[test]
fn for_body_module_call_diagnostics_are_emitted_once_per_authored_site() {
    let main = r#"let m = use { source = './m' }
for x in ['a', 'b', 'c'] {
  m { nn = 1 }
}
"#;
    let parsed = carina_core::parser::parse_file(
        main,
        std::path::Path::new("main.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("fixture must parse");
    assert_eq!(parsed.module_calls.len(), 3);
    assert!(
        parsed
            .module_calls
            .windows(2)
            .all(|calls| calls[0].source_span() == calls[1].source_span())
    );
    assert!(parsed.module_calls.windows(2).all(|calls| {
        calls[0]
            .authored_argument("nn")
            .map(|argument| argument.key_span())
            == calls[1]
                .authored_argument("nn")
                .map(|argument| argument.key_span())
    }));

    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("providers.crn", "provider mock {}\n"),
        ("m/main.crn", "arguments {\n  label: String\n}\n"),
    ]);
    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let unknown = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Unknown argument 'nn'"))
        .collect::<Vec<_>>();
    let missing = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("Missing required argument 'label'")
        })
        .collect::<Vec<_>>();

    assert_eq!(unknown.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(missing.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(
        unknown[0].range,
        Range::new(Position::new(2, 6), Position::new(2, 8))
    );
    assert_eq!(
        missing[0].range,
        Range::new(Position::new(2, 2), Position::new(2, 14))
    );
}

#[test]
fn module_call_reports_literal_union_and_bool_value_mismatches() {
    let main = r#"let web_tier = use { source = './modules/web_tier' }

web_tier {
    environment = 'production'
    enable = 'yes'
}
"#;
    let module = r#"arguments {
    environment: 'dev' | 'stg' | 'prod'
    enable: Bool
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", main), ("modules/web_tier/main.crn", module)]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let type_errors = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("expected 'dev' | 'stg' | 'prod'")
                || diagnostic
                    .message
                    .contains("expected Bool, got string 'yes'")
        })
        .collect::<Vec<_>>();

    assert_eq!(type_errors.len(), 2, "diagnostics: {diagnostics:#?}");
    assert!(
        type_errors.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected 'dev' | 'stg' | 'prod', got string 'production'")),
        "literal-union mismatch missing: {diagnostics:#?}"
    );
    assert!(
        type_errors.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected Bool, got string 'yes'. Use true or false.")),
        "Bool mismatch missing: {diagnostics:#?}"
    );
}

#[test]
fn module_output_reference_mismatch_uses_pre_expansion_signature_and_authored_names() {
    let main = r#"let m = use { source = './modules/m' }
let x = m { n = 1 }
let y = m { n = x.s }
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("modules/m/arguments.crn", "arguments { n: Int }\n"),
        (
            "modules/m/attributes.crn",
            "attributes {\n  s: String = 'str'\n}\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("module call 'y'")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
                && diagnostic.message.contains("from x.s")
        })
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 1, "diagnostics: {diagnostics:#?}");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("module call 'm_")),
        "diagnostics must never expose synthetic instance hashes: {diagnostics:#?}",
    );
}

#[test]
fn module_call_uses_argument_type_declared_in_sibling_file() {
    let main = r#"let inner = use { source = './modules/inner' }

inner {
    name = count
}
"#;
    let temp = write_tag_key_fixture(&[
        ("arguments.crn", "arguments {\n    count: Int\n}\n"),
        ("main.crn", main),
        (
            "modules/inner/main.crn",
            "arguments {\n    name: String\n}\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("cannot assign Int to 'name': expected String, got Int (from count)")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mismatches.len(),
        1,
        "BindingIndex must report the merged directory argument type once: {diagnostics:#?}"
    );
}

#[test]
fn module_call_uses_values_resolved_from_sibling_let_bindings() {
    let main = r#"let inner = use { source = './modules/inner' }

inner {
    count = my_count
    environment = my_env
    names = my_names
    enable = my_flag
}
"#;
    let values = r#"let my_count = 3
let my_env = 'dev'
let my_names = ['a', 'b']
let my_flag = true
"#;
    let module = r#"arguments {
    count: Int
    environment: 'dev' | 'prod'
    names: list(String)
    enable: Bool
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("values.crn", values),
        ("modules/inner/main.crn", module),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
        })
        .collect::<Vec<_>>();
    assert!(
        warnings.is_empty(),
        "directory-resolved sibling let values must not warn: {diagnostics:#?}"
    );
}

#[test]
fn export_uses_value_resolved_from_sibling_let_binding() {
    let exports = "exports {\n    c: Int = my_count\n}\n";
    let temp = write_tag_key_fixture(&[
        ("values.crn", "let my_count = 3\n"),
        ("exports.crn", exports),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "exports.crn", exports);
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
        })
        .collect::<Vec<_>>();
    assert!(
        warnings.is_empty(),
        "directory-resolved sibling export values must not warn: {diagnostics:#?}"
    );
}

#[test]
fn authored_export_span_anchors_sibling_resolved_type_warning() {
    let exports = "exports {\n    count: Int = my_count\n}\n";
    let temp = write_tag_key_fixture(&[
        ("values.crn", "let my_count = 'wrong'\n"),
        ("exports.crn", exports),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "exports.crn", exports);
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("expected Int, got string 'wrong'")
        })
        .unwrap_or_else(|| panic!("missing export type warning: {diagnostics:#?}"));

    assert_eq!(
        warning.range,
        Range::new(Position::new(1, 4), Position::new(1, 9))
    );
}

#[test]
fn attribute_uses_value_resolved_from_sibling_let_binding() {
    let attributes = "attributes {\n    c: Int = my_count\n}\n";
    let temp = write_tag_key_fixture(&[
        ("values.crn", "let my_count = 3\n"),
        ("attributes.crn", attributes),
    ]);

    let diagnostics =
        analyze_with_buffer(&test_engine(), temp.path(), "attributes.crn", attributes);
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
        })
        .collect::<Vec<_>>();
    assert!(
        warnings.is_empty(),
        "directory-resolved sibling attribute values must not warn: {diagnostics:#?}"
    );
}

#[test]
fn module_call_value_typecheck_skips_buffer_guess_when_directory_merge_fails() {
    let main = r#"let inner = use { source = './modules/inner' }

inner {
    environment = the_env
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("values.crn", "let the_env = 'dev'\n"),
        ("broken.crn", "let broken = {\n"),
        (
            "modules/inner/main.crn",
            "arguments {\n    environment: String\n}\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("expected String")
                || !diagnostic.message.contains("the_env")
        }),
        "a buffer-only enum guess must not be value-type-checked: {diagnostics:#?}"
    );
}

#[test]
fn broken_sibling_still_checks_context_free_module_literals_only() {
    let main = r#"let inner = use { source = './modules/inner' }

inner {
    count = 'wrong'
    environment = the_env
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("values.crn", "let the_env = 'dev'\n"),
        ("broken.crn", "let broken = {\n"),
        (
            "modules/inner/main.crn",
            "arguments {\n    count: Int\n    environment: String\n}\n",
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected Int, got string 'wrong'")),
        "the context-free literal mismatch must survive a broken sibling: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("expected String")
                || !diagnostic.message.contains("the_env")
        }),
        "the sibling-dependent value must remain quiet: {diagnostics:#?}"
    );
}

#[test]
fn broken_sibling_uses_source_local_module_reference_fallback() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let main = r#"provider mock {}
let inner = use { source = './modules/inner' }
let r = mock.test.resource { name = 'r' }

inner {
    count = r.name
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        ("broken.crn", "let broken = {\n"),
        ("modules/inner/main.crn", "arguments {\n    count: Int\n}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&engine, temp.path(), "main.crn", main);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("module call 'inner (anonymous call)'")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        }),
        "the source-local BindingIndex fallback must still fire: {diagnostics:#?}"
    );
}

#[test]
fn user_function_reference_arguments_and_returns_use_binding_index_types() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let source = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn takes_int(x: Int) { 'fixed' }
fn returns_int(): Int { r.name }
fn returns_string(): String { r.name }
let a = mock.test.resource { name = takes_int(r.name) }
let b = mock.test.resource { name = returns_int() }
let c = mock.test.resource { name = returns_string() }
"#;
    let temp = write_tag_key_fixture(&[("main.crn", source)]);

    let diagnostics = analyze_with_buffer(&engine, temp.path(), "main.crn", source);
    let function_mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("function '")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        function_mismatches.len(),
        2,
        "function argument and return mismatches must both surface, while the matching return stays quiet: {diagnostics:#?}"
    );
}

#[test]
fn nested_user_function_return_mismatch_surfaces_across_sibling_files() {
    let root = tempfile::tempdir().unwrap();
    let main = "fn f(x: Int): String { 'ok' }\nlet result = f(g(1))\n";
    std::fs::write(root.path().join("main.crn"), main).unwrap();
    std::fs::write(
        root.path().join("funcs.crn"),
        "fn g(y: Int): String { 'str' }\n",
    )
    .unwrap();

    let diagnostics = analyze_with_buffer(&test_engine(), root.path(), "main.crn", main);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("function 'f'")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        }),
        "the directory-merged function table must type g() before f evaluates: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("\"str\"")),
        "evaluated nested-call contents must stay redacted: {diagnostics:#?}"
    );
}

#[test]
fn sibling_function_bare_handle_error_is_owned_by_the_function_document() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let functions =
        "# the call lives in a sibling file\nfn identity(r: mock.test.resource): String { r }\n";
    let temp = write_tag_key_fixture(&[
        ("functions.crn", functions),
        (
            "main.crn",
            "let vpc = mock.test.resource { name = 'vpc' }\nlet bad = identity(vpc)\n",
        ),
        ("providers.crn", "provider mock {}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&engine, temp.path(), "functions.crn", functions);
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic
                .message
                .contains("'r' is a resource handle, not a value")
        })
        .unwrap_or_else(|| panic!("missing sibling function handle diagnostic: {diagnostics:#?}"));
    assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(diagnostic.range.start.line, 1);
}

#[test]
fn sibling_function_handle_field_access_is_valid_in_the_directory_engine() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let functions = "fn name_of(r: mock.test.resource): String { r.name }\n";
    let temp = write_tag_key_fixture(&[
        ("functions.crn", functions),
        (
            "main.crn",
            "let vpc = mock.test.resource { name = 'vpc' }\nlet good = name_of(vpc)\n",
        ),
        ("providers.crn", "provider mock {}\n"),
    ]);

    let diagnostics = analyze_with_buffer(&engine, temp.path(), "functions.crn", functions);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("resource handle, not a value")
                && !diagnostic.message.contains("Unknown function")
        }),
        "valid sibling handle access produced diagnostics: {diagnostics:#?}",
    );
}

fn structural_ref_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("compute.Instance")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("arn", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    custom_engine(schemas)
}

#[test]
fn deferred_sibling_function_type_error_is_one_call_site_error() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("functions.crn"),
        "fn name_of(r: mock.compute.Instance): String { r.name }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("bindings.crn"),
        "let wrong = mock.test.resource { name = \"wrong\" }\n",
    )
    .unwrap();
    let source = "mock.test.resource {\n  name = \"consumer\"\n  parent = name_of(wrong)\n}\n";
    std::fs::write(root.path().join("consumer.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(
        &structural_ref_engine(),
        root.path(),
        "consumer.crn",
        source,
    );
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("mock.compute.Instance")
                && diagnostic.message.contains("mock.test.resource")
                && diagnostic.message.contains("name_of")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        matching[0].range.start.line, 2,
        "diagnostic: {:#?}",
        matching[0]
    );
}

#[test]
fn builtin_handle_error_is_reported_at_the_call_site() {
    let root = tempfile::tempdir().unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nmock.test.resource {\n  name = upper(vpc)\n}\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
    assert!(!matching[0].message.contains("upper(): upper:"));
}

#[test]
fn module_parameter_dependency_and_ref_attribute_are_valid_in_module_scope() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("arguments.crn"),
        "arguments { r: mock.compute.Instance }\n",
    )
    .unwrap();
    let source = "mock.test.resource {\n  name = \"consumer\"\n  parent = r.name\n  directives { depends_on = [r] }\n}\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("depends_on")
                && !diagnostic
                    .message
                    .contains("cannot apply access .name to mock.compute.Instance")
        }),
        "valid typed module-parameter usage produced diagnostics: {diagnostics:#?}",
    );
}

#[test]
fn module_parameter_function_mismatch_is_an_error_in_the_root_document() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { r: mock.compute.Instance }\n",
    )
    .unwrap();
    std::fs::write(
        module.join("functions.crn"),
        "fn name_of(x: mock.test.resource): String { x.name }\n",
    )
    .unwrap();
    std::fs::write(
        module.join("main.crn"),
        "mock.test.resource {\n  name = \"consumer\"\n  parent = name_of(r)\n}\n",
    )
    .unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet imported = use { source = \"./module\" }\nimported { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("mock.test.resource")
                && diagnostic.message.contains("mock.compute.Instance")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        matching[0].message.contains("module/main.crn"),
        "diagnostic should name the module-owned source file: {:#?}",
        matching[0]
    );
    assert!(
        !matching[0]
            .message
            .contains(&root.path().display().to_string()),
        "diagnostic source path must be relative to the configuration root: {:#?}",
        matching[0]
    );
}

#[test]
fn non_resource_handle_module_argument_is_an_error_at_the_root_call() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("upstream")).unwrap();
    let module = root.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments { r: mock.compute.Instance }\nmock.test.resource { name = r.name }\n",
    )
    .unwrap();
    let source = "let up = upstream_state { source = \"./upstream\" }\nlet imported = use { source = \"./module\" }\nimported { r = up }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("'up' is an upstream state")
                && diagnostic
                    .message
                    .contains("not a resource of type mock.compute.Instance")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
}

#[test]
fn handle_backed_module_constraint_failure_is_an_error_in_the_lsp() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("checked");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { r: mock.compute.Instance }\n",
    )
    .unwrap();
    std::fs::write(
        module.join("constraints.crn"),
        "require r.name == \"nope\", \"wrong resource name\"\n",
    )
    .unwrap();
    std::fs::write(
        module.join("resource.crn"),
        "mock.test.resource { name = \"consumer\" parent = r.name }\n",
    )
    .unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet checked = use { source = \"./checked\" }\nchecked { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("wrong resource name"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn nested_handle_backed_module_constraint_failure_is_an_lsp_error() {
    let root = tempfile::tempdir().unwrap();
    let checked = root.path().join("checked");
    let outer = root.path().join("outer");
    std::fs::create_dir(&checked).unwrap();
    std::fs::create_dir(&outer).unwrap();
    std::fs::write(
        checked.join("main.crn"),
        "arguments { inner: mock.compute.Instance }\nrequire inner.name == \"nope\", \"wrong nested resource name\"\n",
    )
    .unwrap();
    std::fs::write(
        outer.join("main.crn"),
        "arguments { r: mock.compute.Instance }\nlet checked = use { source = \"../checked\" }\nchecked { inner = r }\n",
    )
    .unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet outer = use { source = \"./outer\" }\nouter { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("wrong nested resource name"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
}

#[test]
fn bare_handle_in_module_constraint_is_a_located_lsp_error() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("checked");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { r: mock.compute.Instance }\n",
    )
    .unwrap();
    let constraint = "require\n  length(r) > 0,\n  \"bare handle\"\n";
    std::fs::write(module.join("constraints.crn"), constraint).unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet checked = use { source = \"./checked\" }\nchecked { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("'r' is a resource handle, not a value")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
    assert!(
        matching[0].message.contains("checked/constraints.crn")
            && matching[0].message.contains("line 2"),
        "diagnostic must name the module-owned constraint use site: {:#?}",
        matching[0]
    );
}

#[test]
fn bare_handle_in_argument_validation_is_a_located_lsp_error() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("checked");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        r#"arguments {
  r: mock.compute.Instance {
    validation {
      condition = length(r) > 0
      error_message = "bare handle"
    }
  }
}
"#,
    )
    .unwrap();
    let source = "let vpc = mock.compute.Instance { name = \"vpc\" }\nlet checked = use { source = \"./checked\" }\nchecked { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("'r' is a resource handle, not a value")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
    assert!(
        matching[0].message.contains("checked/arguments.crn")
            && matching[0].message.contains("line 4"),
        "diagnostic must name the argument validation expression: {:#?}",
        matching[0]
    );
}

#[test]
fn wrong_resource_handle_module_argument_is_an_lsp_error() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments { r: mock.compute.Instance }\nmock.test.resource { name = r.name }\n",
    )
    .unwrap();
    let source = "let wrong = mock.test.resource { name = \"wrong\" }\nlet imported = use { source = \"./module\" }\nimported { r = wrong }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("mock.compute.Instance")
                && diagnostic.message.contains("mock.test.resource")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 2);
}

#[test]
fn sibling_handle_if_condition_is_a_located_lsp_error() {
    let root = tempfile::tempdir().unwrap();
    let source = "let selected = if vpc { mock.test.resource { name = \"yes\" } } else { mock.test.resource { name = \"no\" } }\n";
    std::fs::write(root.path().join("b.crn"), source).unwrap();
    std::fs::write(
        root.path().join("a.crn"),
        "let vpc = mock.compute.Instance { name = \"vpc\" }\n",
    )
    .unwrap();

    let diagnostics = analyze_with_buffer(&structural_ref_engine(), root.path(), "b.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("'vpc' is a resource, not a value")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(matching[0].range.start.line, 0);
}

#[test]
fn sibling_user_function_for_iterable_uses_the_directory_function_table() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("functions.crn"),
        "fn names(r): list(String) { [\"a\", \"b\"] }\n",
    )
    .unwrap();
    let source = "for n in names(\"x\") {\n  mock.test.resource {\n    name = n\n  }\n}\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic
                .message
                .contains("Unknown built-in function: names")
                && !diagnostic
                    .message
                    .contains("for iterable function call 'names' failed")
        }),
        "the directory-defined function must drive the iterable: {diagnostics:#?}",
    );
}

fn sibling_for_before_anonymous_resource_diagnostics(
    for_count: &str,
    trailing_count: &str,
) -> Vec<Diagnostic> {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::required("count", AttributeType::int()).build())
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("values.crn"),
        "provider mock {}\nlet sibling_values = [\"one\", \"two\"]\n",
    )
    .unwrap();
    let source = format!(
        "for ignored in sibling_values {{\n  mock.test.resource {{\n    count = {for_count}\n  }}\n}}\n\nmock.test.resource {{\n  count = {trailing_count}\n}}\n"
    );
    std::fs::write(root.path().join("main.crn"), &source).unwrap();

    analyze_with_buffer(&engine, root.path(), "main.crn", &source)
}

#[test]
fn sibling_expanded_for_does_not_shift_invalid_value_onto_valid_anonymous_resource() {
    let diagnostics = sibling_for_before_anonymous_resource_diagnostics("\"wrong\"", "1");
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("expected Int"))
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(mismatches[0].range.start.line, 2);
}

#[test]
fn sibling_expanded_for_does_not_hide_invalid_trailing_anonymous_resource() {
    let diagnostics = sibling_for_before_anonymous_resource_diagnostics("1", "\"wrong\"");
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("expected Int"))
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(mismatches[0].range.start.line, 7);
}

#[test]
fn static_union_uses_function_value_whose_argument_is_a_sibling_file_let() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required(
                    "mode",
                    AttributeType::union(vec![
                        AttributeType::enum_(
                            TypeIdentity::bare("Mode"),
                            Some(vec!["active".to_string(), "passive".to_string()]),
                            vec![],
                            None,
                            None,
                        ),
                        AttributeType::int(),
                    ]),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid"),
    );
    let engine = custom_engine(schemas);
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("values.crn"),
        "provider test {}\nlet sibling_mode = \"invalid\"\n",
    )
    .unwrap();
    let source = "let target = test.test.resource {\n  mode = upper(sibling_mode)\n}\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&engine, root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.message.contains("expects an enum identifier")
                && diagnostic.message.contains("active, passive")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].range.start.line, 1);
}

#[test]
fn nested_forward_declared_user_function_return_mismatch_surfaces() {
    let root = tempfile::tempdir().unwrap();
    let main = r#"fn f(x: Int): String { 'ok' }
let result = f(g(1))
fn g(y: Int): String { 'str' }
"#;
    std::fs::write(root.path().join("main.crn"), main).unwrap();

    let diagnostics = analyze_with_buffer(&test_engine(), root.path(), "main.crn", main);
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("function 'f'")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        }),
        "forward declaration order must not bypass the parameter check: {diagnostics:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("\"str\"")),
        "evaluated nested-call contents must stay redacted: {diagnostics:#?}"
    );
}

#[test]
fn export_value_typecheck_skips_buffer_guess_when_directory_merge_fails() {
    let exports = "exports {\n    bad: Int = 'wrong'\n    e1: String = the_env\n}\n";
    let temp = write_tag_key_fixture(&[
        ("values.crn", "let the_env = 'dev'\n"),
        ("broken.crn", "let broken = {\n"),
        ("exports.crn", exports),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "exports.crn", exports);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected Int, got string 'wrong'")),
        "a context-free export literal must still be checked: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("expected String")
                || !diagnostic.message.contains("the_env")
        }),
        "a buffer-only export value guess must not be type-checked: {diagnostics:#?}"
    );
}

#[test]
fn attribute_value_typecheck_skips_buffer_guess_when_directory_merge_fails() {
    let attributes = "attributes {\n    bad: Int = 'wrong'\n    e1: String = the_env\n}\n";
    let temp = write_tag_key_fixture(&[
        ("arguments.crn", "arguments {\n    input: String\n}\n"),
        ("values.crn", "let the_env = 'dev'\n"),
        ("broken.crn", "let broken = {\n"),
        ("attributes.crn", attributes),
    ]);

    let diagnostics =
        analyze_with_buffer(&test_engine(), temp.path(), "attributes.crn", attributes);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("expected Int, got string 'wrong'")),
        "a context-free attribute literal must still be checked: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("expected String")
                || !diagnostic.message.contains("the_env")
        }),
        "a buffer-only attribute value guess must not be type-checked: {diagnostics:#?}"
    );
}

#[test]
fn export_argument_type_mismatch_is_reported_once_by_binding_index() {
    let source = r#"arguments {
    count: Int
}

exports {
    z: Bool = count
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", source)]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", source);
    let type_mismatches = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("expected Bool"))
        .collect::<Vec<_>>();
    assert_eq!(
        type_mismatches.len(),
        1,
        "BindingIndex must be the sole authority for reference-valued exports: {diagnostics:#?}"
    );
    assert!(
        type_mismatches[0].message.contains("export 'z'"),
        "the sole diagnostic must come from BindingIndex: {diagnostics:#?}"
    );
}

fn tag_key_style_diagnostics(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> Vec<&tower_lsp::lsp_types::Diagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.starts_with("Tag key '"))
        .collect()
}

#[test]
fn tag_key_style_warns_on_current_buffer_minority_key_from_sibling_population() {
    let main = r#"let app = awscc.ec2.Vpc {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "db.crn",
            r#"let db = awscc.rds.Db {
    tags = {
        Name = "database"
        Environment = "production"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(style_diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    let diagnostic = style_diagnostics[0];
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.start.character, 8);
    assert_eq!(diagnostic.range.end.character, 18);
    assert_eq!(
        diagnostic.message,
        "Tag key 'managed_by' doesn't match the dominant style (PascalCase). Use consistent casing for tag keys."
    );
}

#[test]
fn tag_key_style_diagnostics_are_owned_by_the_current_buffer() {
    let main = r#"let app = awscc.ec2.Vpc {
    tags = {
        Name = "application"
        Environment = "production"
    }
}
"#;
    let db = r#"let db = awscc.rds.Db {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", main), ("db.crn", db)]);

    let main_diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    assert!(
        tag_key_style_diagnostics(&main_diagnostics).is_empty(),
        "the majority-style buffer must not own its sibling's warning: {main_diagnostics:#?}"
    );

    let db_diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "db.crn", db);
    let db_style_diagnostics = tag_key_style_diagnostics(&db_diagnostics);
    assert_eq!(
        db_style_diagnostics.len(),
        1,
        "the sibling's own pass must anchor its minority key: {db_diagnostics:#?}"
    );
    assert_eq!(db_style_diagnostics[0].range.start.line, 2);
    assert_eq!(db_style_diagnostics[0].range.start.character, 8);
}

#[test]
fn tag_key_style_population_includes_called_module_directories() {
    let main = r#"let standard_tags = use { source = "./modules/standard-tags" }
let defaults = standard_tags {}

let app = awscc.ec2.Vpc {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "modules/standard-tags/main.crn",
            r#"attributes {
    tags = {
        Name = "defaults"
        Environment = "production"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(style_diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(style_diagnostics[0].range.start.line, 5);
    assert_eq!(
        style_diagnostics[0].message,
        "Tag key 'managed_by' doesn't match the dominant style (PascalCase). Use consistent casing for tag keys."
    );
}

#[test]
fn tag_key_style_population_uses_unsaved_current_buffer_text() {
    let on_disk_main = r#"let app = awscc.ec2.Vpc {
    tags = {
        Name = "application"
        Environment = "production"
    }
}
"#;
    let unsaved_main = r#"let app = awscc.ec2.Vpc {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", on_disk_main),
        (
            "db.crn",
            r#"let db = awscc.rds.Db {
    tags = {
        Name = "database"
        Environment = "production"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", unsaved_main);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(style_diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(style_diagnostics[0].range.start.line, 2);
    assert_eq!(
        style_diagnostics[0].message,
        "Tag key 'managed_by' doesn't match the dominant style (PascalCase). Use consistent casing for tag keys."
    );
}

#[test]
fn tag_key_style_consistent_across_root_and_module_files_has_no_diagnostic() {
    let main = r#"let standard_tags = use { source = "./modules/standard-tags" }
let defaults = standard_tags {}

let app = awscc.ec2.Vpc {
    tags = {
        Name = "application"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "db.crn",
            r#"let db = awscc.rds.Db {
    tags = {
        Environment = "production"
    }
}
"#,
        ),
        (
            "modules/standard-tags/main.crn",
            r#"attributes {
    tags = {
        ManagedBy = "carina"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);

    assert!(
        tag_key_style_diagnostics(&diagnostics).is_empty(),
        "consistent styles must not warn: {diagnostics:#?}"
    );
}

#[test]
fn tag_key_style_warns_when_current_buffer_is_inside_module_directory() {
    let module_main = r#"attributes {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("modules/net/main.crn", module_main),
        (
            "modules/net/defaults.crn",
            r#"let defaults = awscc.ec2.Vpc {
    tags = {
        Name = "network"
        Environment = "production"
    }
}
"#,
        ),
    ]);
    let module_dir = temp.path().join("modules/net");

    let diagnostics = analyze_with_buffer(&test_engine(), &module_dir, "main.crn", module_main);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(style_diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(style_diagnostics[0].range.start.line, 2);
    assert_eq!(style_diagnostics[0].range.start.character, 8);
    assert_eq!(style_diagnostics[0].range.end.line, 2);
    assert_eq!(style_diagnostics[0].range.end.character, 18);
}

#[test]
fn tag_key_style_same_basename_module_warning_does_not_anchor_in_root_buffer() {
    let root_main = r#"let net = use { source = "./modules/net" }
let network = net {}

let app = awscc.ec2.Vpc {
    tags = {
        Name = "application"
        Environment = "production"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", root_main),
        (
            "modules/net/main.crn",
            r#"attributes {
    tags = {
        managed_by = "carina"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", root_main);

    assert!(
        tag_key_style_diagnostics(&diagnostics).is_empty(),
        "modules/net/main.crn must not own root main.crn diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn tag_key_style_population_deduplicates_module_imported_by_two_root_files() {
    let main = r#"let first = use { source = "./modules/shared" }
let first_defaults = first {}

let app = awscc.ec2.Vpc {
    tags = {
        managed_by = "carina"
        cost_center = "infra"
        Owner = "team"
    }
}
"#;
    let temp = write_tag_key_fixture(&[
        ("main.crn", main),
        (
            "db.crn",
            r#"let second = use { source = "./modules/shared" }
let second_defaults = second {}

let db = awscc.rds.Db {
    tags = {
        environment_name = "production"
        service_name = "database"
    }
}
"#,
        ),
        (
            "modules/shared/main.crn",
            r#"attributes {
    tags = {
        Name = "defaults"
        Environment = "production"
    }
}
"#,
        ),
    ]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "main.crn", main);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(
        style_diagnostics.len(),
        1,
        "the 4-snake/3-Pascal population must emit one current-buffer warning: {diagnostics:#?}"
    );
    assert_eq!(style_diagnostics[0].range.start.line, 7);
    assert_eq!(style_diagnostics[0].range.start.character, 8);
    assert!(style_diagnostics[0].message.starts_with("Tag key 'Owner'"));
    assert!(style_diagnostics[0].message.contains("(snake_case)"));
}

#[test]
fn tag_key_style_population_deduplicates_self_referential_root_directory() {
    let main = r#"let self_module = use { source = "." }
let recursive = self_module {}

let app = awscc.ec2.Vpc {
    tags = {
        Name = "application"
        Environment = "production"
    }
}
"#;
    let db = r#"let db = awscc.rds.Db {
    tags = {
        managed_by = "carina"
    }
}
"#;
    let temp = write_tag_key_fixture(&[("main.crn", main), ("db.crn", db)]);

    let diagnostics = analyze_with_buffer(&test_engine(), temp.path(), "db.crn", db);
    let style_diagnostics = tag_key_style_diagnostics(&diagnostics);

    assert_eq!(
        style_diagnostics.len(),
        1,
        "self-imported root keys must not duplicate diagnostics: {diagnostics:#?}"
    );
    assert_eq!(style_diagnostics[0].range.start.line, 2);
    assert_eq!(style_diagnostics[0].range.start.character, 8);
    assert!(
        style_diagnostics[0]
            .message
            .starts_with("Tag key 'managed_by'")
    );
}

#[test]
fn tag_key_style_anchor_uses_character_column_after_multibyte_whitespace() {
    let temp = tempfile::tempdir().unwrap();
    let current_file = temp.path().join("main.crn");
    let sibling_file = temp.path().join("defaults.crn");
    let current_source = "tags = {\n\u{3000}\u{3000}managed_by = \"carina\"\n}";
    let sibling_source =
        "tags = {\n    Name = \"application\"\n    Environment = \"production\"\n}";
    let merged_result = carina_core::config_loader::DirectoryParseResult {
        parsed: carina_core::parser::ParsedFile::default(),
        authored_bindings: carina_core::config_loader::AuthoredBindingView::parse_file_source(
            "",
            std::path::Path::new("test.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .unwrap(),
        duplicate_declarations: Vec::new(),
        source_files: vec![
            (current_file, current_source.to_string()),
            (sibling_file, sibling_source.to_string()),
        ],
        sole_backend_file: None,
        semantic_files: Vec::new(),
    };

    let diagnostics =
        test_engine().check_mixed_tag_key_styles("main.crn", temp.path(), &merged_result);

    assert_eq!(diagnostics.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(diagnostics[0].range.start.line, 1);
    assert_eq!(diagnostics[0].range.start.character, 2);
    assert_eq!(diagnostics[0].range.end.character, 12);
}

#[test]
fn module_owned_deferred_user_functions_are_not_reclassified_as_builtins() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments { r: String }
fn suffix(value: String): String { "${value}-x" }
fn name_with_suffix(value: String): String { suffix(value) }
attributes { o: String = name_with_suffix(r) }
mock.test.resource { name = "module-resource" }
"#,
    )
    .unwrap();
    let source = r#"let vpc = mock.test.resource { name = "vpc" }
let imported = use { source = "./module" }
let instance = imported { r = vpc.name }
mock.test.resource { name = "consumer" parent = instance.o }
"#;
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic
                .message
                .contains("Unknown built-in function: name_with_suffix")
                && !diagnostic.message.contains("name_with_suffix():")
        }),
        "the module-owned definition must travel with its deferred call: {diagnostics:#?}",
    );
}

#[test]
fn module_output_handle_user_function_keeps_its_definition_in_lsp() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("module");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"arguments { r: mock.test.resource }
fn suffix(value: String): String { "${value}-x" }
fn name_with_suffix(value: mock.test.resource): String { suffix(value.name) }
attributes { o: String = name_with_suffix(r) }
mock.test.resource { name = "module-resource" }
"#,
    )
    .unwrap();
    let source = r#"let vpc = mock.test.resource { name = "vpc" }
let imported = use { source = "./module" }
let instance = imported { r = vpc }
mock.test.resource { name = "consumer" parent = instance.o }
"#;
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains("Unknown built-in function")
                && !diagnostic.message.contains("Unknown function")
                && !diagnostic.message.contains("resource handle, not a value")
        }),
        "the handle-valued call must retain its module definition: {diagnostics:#?}",
    );
}

#[test]
fn deferred_module_user_function_body_error_is_reported_by_lsp() {
    let module = tempfile::tempdir().unwrap();
    let source = r#"arguments { r: String }
fn name_with_suffix(value: String): String { missing_helper(value) }
attributes { o: String = name_with_suffix(r) }
"#;
    std::fs::write(module.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), module.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("missing_helper"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert_eq!(matching[0].range.start.line, 1);
    assert_eq!(matching[0].range.start.character, 45);
}

#[test]
fn module_argument_if_has_no_runtime_condition_diagnostic_when_opened_standalone() {
    let module = tempfile::tempdir().unwrap();
    let source = r#"arguments { flag: Bool }
fn identity(value: Bool): Bool { value }
attributes {
  selected: String = if identity(flag) { "yes" } else { "no" }
}
"#;
    std::fs::write(module.path().join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&test_engine(), module.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains(
                "if condition depends on a runtime value; condition must be statically known at parse time",
            )
        }),
        "a standalone module argument is a template dependency, not a runtime condition: {diagnostics:#?}",
    );
}

#[test]
fn module_argument_if_validates_both_value_branches_when_opened_standalone() {
    let module = tempfile::tempdir().unwrap();
    let source = r#"arguments { flag: Bool }
attributes {
  selected: String = if flag { "yes" } else { 42 }
}
"#;
    std::fs::write(module.path().join("main.crn"), source).unwrap();

    let diagnostics = analyze_with_buffer(&test_engine(), module.path(), "main.crn", source);
    assert!(
        diagnostics.iter().all(|diagnostic| {
            !diagnostic.message.contains(
                "if condition depends on a runtime value; condition must be statically known at parse time",
            )
        }),
        "the unresolved module-only condition itself must not be diagnosed: {diagnostics:#?}",
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'selected': type mismatch")
                || (diagnostic.message.contains("expected String")
                    && diagnostic.message.contains("got int"))
        }),
        "the unselected-at-template-time else branch must still be type-checked: {diagnostics:#?}",
    );
}

#[test]
fn nested_forwarded_handle_remains_a_dependency_operand_in_lsp() {
    let root = tempfile::tempdir().unwrap();
    let outer = root.path().join("outer");
    let inner = root.path().join("inner");
    std::fs::create_dir(&outer).unwrap();
    std::fs::create_dir(&inner).unwrap();
    let outer_source = "arguments { r: mock.test.resource }\nlet inner = use { source = \"../inner\" }\ninner { x = r }\n";
    std::fs::write(outer.join("main.crn"), outer_source).unwrap();
    std::fs::write(
        inner.join("main.crn"),
        "arguments { x: mock.test.resource }\nmock.test.resource {\n  name = \"consumer\"\n  directives { depends_on = [x] }\n}\n",
    )
    .unwrap();
    let source = "let vpc = mock.test.resource { name = \"vpc\" }\nlet outer = use { source = \"./outer\" }\nouter { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    for (base, filename, buffer) in [
        (root.path(), "main.crn", source),
        (outer.as_path(), "main.crn", outer_source),
    ] {
        let diagnostics = analyze_with_buffer(&structural_ref_engine(), base, filename, buffer);
        assert!(
            diagnostics.iter().all(|diagnostic| {
                !diagnostic
                    .message
                    .contains("depends_on parameter 'x' received a value")
            }),
            "a forwarded parameter is still pending handle classification: {diagnostics:#?}",
        );
    }
}

#[test]
fn nested_bare_handle_lsp_error_uses_the_inner_operand_location() {
    let root = tempfile::tempdir().unwrap();
    let outer = root.path().join("outer");
    let inner = root.path().join("inner");
    std::fs::create_dir(&outer).unwrap();
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(
        outer.join("main.crn"),
        "arguments { r: mock.test.resource }\nlet inner = use { source = \"../inner\" }\n\n\ninner { x = r }\n",
    )
    .unwrap();
    std::fs::write(
        inner.join("main.crn"),
        "arguments { x: mock.test.resource }\n\n\n\n\nmock.test.resource {\n  name = \"consumer\"\n  parent = x\n}\n",
    )
    .unwrap();
    let source = "let vpc = mock.test.resource { name = \"vpc\" }\nlet outer = use { source = \"./outer\" }\nouter { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("resource handle, not a value"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert!(
        matching[0].message.contains("inner/main.crn")
            && matching[0].message.contains("line 8")
            && matching[0].message.contains("'x' is a resource handle"),
        "the nested diagnostic must retain one coherent owner: {:#?}",
        matching[0],
    );
    assert!(
        !matching[0].message.contains("outer/../inner")
            && !matching[0].message.contains("line 5: 'r'"),
        "mixed path/line provenance leaked: {:#?}",
        matching[0],
    );
}

#[test]
fn handle_constraint_lsp_failure_reports_the_selected_operand() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("checked");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments { r: mock.test.resource }\nrequire r.name == \"nope\", \"bad name\"\nmock.test.resource { name = \"consumer\" parent = r.name }\n",
    )
    .unwrap();
    let source = "let vpc = mock.test.resource { name = \"vpc\" }\nlet checked = use { source = \"./checked\" }\nchecked { r = vpc }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    let matching = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("bad name"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
    assert!(
        matching[0].message.contains("r.name =") && !matching[0].message.contains("got r = vpc"),
        "the selected operand must own the actual value: {:#?}",
        matching[0],
    );
}

#[test]
fn validation_subscript_uses_the_regular_expression_parser_in_lsp() {
    let root = tempfile::tempdir().unwrap();
    let module = root.path().join("checked");
    std::fs::create_dir(&module).unwrap();
    std::fs::write(
        module.join("main.crn"),
        "arguments { values: map(String) }\nrequire values[(\"ONE\")] == \"one\", \"wrong element\"\nmock.test.resource { name = \"consumer\" }\n",
    )
    .unwrap();
    let source = "let checked = use { source = \"./checked\" }\nchecked { values = { \"ONE\" = \"one\" } }\n";
    std::fs::write(root.path().join("main.crn"), source).unwrap();

    let diagnostics =
        analyze_with_buffer(&structural_ref_engine(), root.path(), "main.crn", source);
    assert!(
        diagnostics.is_empty(),
        "a validation subscript must accept a regular constant expression: {diagnostics:#?}",
    );
}
