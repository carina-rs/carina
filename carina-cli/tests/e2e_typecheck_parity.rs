//! End-to-end CLI parity tests for the type-check pipeline (#2247).
//!
//! Each scenario builds the same directory-shaped fixture as
//! `carina-lsp/tests/e2e_typecheck.rs` (#2215), runs both:
//!
//! 1. `DiagnosticEngine::analyze` (LSP path)
//! 2. `validate_with_factories` (CLI path, the production pipeline
//!    behind `carina validate` minus stdout formatting)
//!
//! and asserts the two diagnostic sets agree on count and message
//! substrings. Driving the CLI logic directly — rather than spawning
//! the `carina` binary as a subprocess — lets the test inject the
//! same hand-built schemas the LSP test uses without a WASM provider
//! plugin (the constraint that left `carina-cli/tests/negative_validation.rs`
//! `#[ignore]`-gated).
//!
//! Tests run unconditionally in CI. End-to-end coverage of the binary
//! itself (argument parsing, exit codes, stdout formatting) is left to
//! the acceptance-test suite that exercises the real provider plugins
//! against `carina-rs/infra/`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{ConcreteValue, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    ValidatedResourceSchema, legacy_validator,
};
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;
use indexmap::IndexMap;
use tempfile::TempDir;

// ============================================================================
// Fixture + helpers (mirror carina-lsp/tests/e2e_typecheck.rs)
// ============================================================================

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("write fixture file");
    }
    dir
}

fn engine_with_schemas(schemas: SchemaRegistry) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .iter()
        .map(|(provider, _, _, _)| provider.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let factories = factories_for(&schemas);
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(factories))
}

fn single_schema_map(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    schemas
}

fn fixture_enum_segment_as_aws_value(s: &str) -> String {
    // Mirrors the old last-dot-segment semantics for these dot-free fixture
    // values, then rewrites DSL underscores to AWS hyphens.
    s.split('.').next_back().unwrap_or(s).replace('_', "-")
}

/// Build a `ProviderFactory` set covering every provider name implied by
/// the schemas. The CLI pipeline keys schemas by provider, so each
/// distinct prefix (`test`, `aws`, ...) needs its own factory entry.
fn factories_for(schemas: &SchemaRegistry) -> Vec<Box<dyn ProviderFactory>> {
    let mut by_provider: HashMap<String, Vec<ValidatedResourceSchema>> = HashMap::new();
    for (provider, _resource_type, _kind, schema) in schemas.iter() {
        let provider = provider.to_string();
        by_provider
            .entry(provider)
            .or_default()
            .push(schema.clone());
    }
    by_provider
        .into_iter()
        .map(|(name, schemas)| {
            Box::new(TestProviderFactory { name, schemas }) as Box<dyn ProviderFactory>
        })
        .collect()
}

/// Run the LSP path on a fixture file and return its diagnostics.
fn lsp_diagnostics(
    engine: &DiagnosticEngine,
    fixture: &TempDir,
    file_name: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let path = fixture.path().join(file_name);
    let text = std::fs::read_to_string(&path).expect("read fixture file");
    let doc = Document::new_with_path(
        text,
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    engine.analyze(&doc, Some(&path))
}

/// Run the CLI path on a fixture directory and return the diagnostic
/// strings the CLI would surface.
fn cli_diagnostics(factories: Vec<Box<dyn ProviderFactory>>, fixture: &TempDir) -> Vec<String> {
    cli_diagnostics_at(factories, fixture.path())
}

fn cli_diagnostics_at(
    factories: Vec<Box<dyn ProviderFactory>>,
    path: &std::path::Path,
) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(
        path,
        carina_cli::wiring::LoadedProviderFactory::for_test(factories),
    )
}

fn cli_validated_file(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> carina_core::parser::InferredFile {
    let loaded = carina_core::config_loader::load_configuration_with_config(
        fixture.path(),
        &carina_core::parser::ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .expect("load CLI fixture");
    let carina_core::config_loader::LoadedConfig {
        parsed,
        duplicate_declarations,
        ..
    } = loaded;
    let providers = carina_cli::wiring::ProviderLoad::for_test(
        carina_cli::wiring::LoadedProviderFactory::for_test(factories),
        &parsed.providers,
    );
    let validated = providers
        .validate_evaluated_config(parsed, fixture.path(), false, &duplicate_declarations)
        .expect("CLI validation fixture must pass");
    validated.evaluated_resources_for_test().clone()
}

fn message_multiset<'a>(messages: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for message in messages {
        *counts.entry(message.to_string()).or_default() += 1;
    }
    counts
}

fn lsp_message_multiset(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> BTreeMap<String, usize> {
    message_multiset(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str()),
    )
}

fn cli_located_message_multiset(diagnostics: &[String]) -> BTreeMap<String, usize> {
    message_multiset(diagnostics.iter().map(|diagnostic| {
        if let Some((context, message)) = diagnostic.split_once('\n')
            && context
                .rsplit_once(" at line ")
                .is_some_and(|(_, line)| line.parse::<usize>().is_ok())
        {
            return message;
        }
        match diagnostic.rsplit_once(" at line ") {
            Some((message, line)) if line.parse::<usize>().is_ok() => message,
            Some((_message, _line)) => diagnostic,
            None => diagnostic,
        }
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalDiagnostic {
    message: String,
    severity: &'static str,
}

fn canonical_error_multiset(
    messages: impl IntoIterator<Item = String>,
) -> BTreeMap<CanonicalDiagnostic, usize> {
    let mut counts = BTreeMap::new();
    for message in messages {
        *counts
            .entry(CanonicalDiagnostic {
                message,
                severity: "ERROR",
            })
            .or_default() += 1;
    }
    counts
}

fn cli_located_error_multiset(diagnostics: &[String]) -> BTreeMap<CanonicalDiagnostic, usize> {
    canonical_error_multiset(
        cli_located_message_multiset(diagnostics)
            .into_iter()
            .flat_map(|(message, count)| std::iter::repeat_n(message, count)),
    )
}

fn lsp_error_multiset(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> BTreeMap<CanonicalDiagnostic, usize> {
    canonical_error_multiset(
        diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            })
            .map(|diagnostic| diagnostic.message.clone()),
    )
}

struct ModuleCallParityCase {
    name: &'static str,
    calls: &'static str,
    outer_arguments: &'static str,
    outer_calls: &'static str,
    expected: &'static [&'static str],
    expected_cli_line: Option<usize>,
    expected_lsp_range: Option<((u32, u32), (u32, u32))>,
}

fn module_call_parity_fixture(case: &ModuleCallParityCase) -> TempDir {
    let fixture = tempfile::tempdir().expect("module-call parity fixture");
    let outer = fixture.path().join("modules/outer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"provider test {}
let outer = use { source = "./modules/outer" }
let inner = use { source = "./modules/inner" }
"#,
    )
    .expect("write imports.crn");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        r#"let source = test.r.Source {
  name = "source"
  count = 7
}
"#,
    )
    .expect("write bindings.crn");
    std::fs::write(fixture.path().join("calls.crn"), case.calls).expect("write calls.crn");
    std::fs::write(outer.join("arguments.crn"), case.outer_arguments)
        .expect("write outer arguments");
    std::fs::write(
        outer.join("imports.crn"),
        "let inner = use { source = \"../inner\" }\n",
    )
    .expect("write outer imports");
    std::fs::write(outer.join("calls.crn"), case.outer_calls).expect("write outer calls");
    std::fs::write(
        inner.join("arguments.crn"),
        "arguments {\n  value: Int\n}\n",
    )
    .expect("write inner arguments");
    fixture
}

fn module_call_parity_schemas() -> SchemaRegistry {
    single_schema_map(
        ResourceSchema::new("r.Source")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(AttributeSchema::required("count", AttributeType::int()).build()),
    )
}

fn assert_module_resolution_expands(base_dir: &std::path::Path, schemas: &SchemaRegistry) {
    let context = carina_core::parser::ProviderContext {
        decryptor: None,
        custom_types: carina_core::provider::provider_custom_types_from_factories(Arc::new(
            factories_for(schemas),
        )),
        resource_types: carina_core::parser::ProviderContext::resource_types_from_schema_registry(
            schemas,
        ),
    };
    let directory = carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
        base_dir,
        &context,
        &HashMap::new(),
    )
    .expect("the fixture directory must parse before module expansion");
    let signatures = carina_core::module_resolver::load_resolved_module_signatures(
        &directory.parsed,
        base_dir,
        &context,
    );
    let binding_use_report = carina_core::validation::check_bare_binding_uses(
        &directory.parsed,
        &carina_core::validation::ValidationTypeContext::new(
            &directory.parsed,
            schemas,
            &context,
            &signatures,
        ),
    );
    let bindings = carina_core::binding_index::BindingIndex::from_parsed_with_module_signatures(
        &directory.parsed,
        schemas,
        &signatures,
    );
    let covered_findings = carina_core::validation::check_module_calls(
        carina_core::validation::ModuleCallCheckInput {
            authored: &directory.parsed,
            semantic: Some(&directory.parsed),
            base_dir: Some(base_dir),
            imported_signatures: &signatures,
            provider_context: &context,
            schemas,
            binding_use_report: &binding_use_report,
            bindings: &bindings,
            reference_checks: carina_core::validation::ModuleCallReferenceChecks::Enabled,
        },
    );
    match carina_core::module_resolver::resolve_modules_with_diagnostics(
        &directory.parsed,
        base_dir,
        &context,
        schemas,
        &covered_findings,
    ) {
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::Expanded { graph } => {
            assert!(
                !graph.compositions.is_empty(),
                "the expansion proof must include a composition"
            );
        }
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::ConstraintFailed {
            findings,
            errors,
        } => panic!(
            "fixture must reach expanded diagnostics; constraint findings={findings:#?}, residual count={}",
            errors.len(),
        ),
        carina_core::module_resolver::ModuleResolutionDiagnosticResult::Unowned { errors } => {
            panic!(
                "fixture must reach expanded diagnostics; residual count={}",
                errors.len()
            )
        }
    }
}

#[test]
fn module_call_failures_have_exact_cli_lsp_error_parity_across_directory_files() {
    let cases = [
        ModuleCallParityCase {
            name: "unknown argument",
            calls: "outer {\n  value = \"ok\"\n  surprise = true\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &["Unknown argument 'surprise' for module 'outer'"],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "missing required argument",
            calls: "outer {}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &["Missing required argument 'value' for module 'outer'"],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "ordinary type mismatch",
            calls: "outer {\n  count = \"not-an-int\"\n}\n",
            outer_arguments: "arguments {\n  count: Int\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'count': expected Int, got string 'not-an-int'",
            ],
            expected_cli_line: Some(2),
            expected_lsp_range: Some(((1, 10), (1, 22))),
        },
        ModuleCallParityCase {
            name: "attribute reference mismatch",
            calls: "outer {\n  value = source.count\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from source.count)",
            ],
            expected_cli_line: Some(2),
            expected_lsp_range: Some(((1, 10), (1, 22))),
        },
        ModuleCallParityCase {
            name: "nested outer to inner mismatch",
            calls: "outer {\n  value = \"root\"\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "inner {\n  value = value\n}\n",
            expected: &[
                "module call 'inner (anonymous call)': argument 'value': cannot assign String to 'value': expected Int, got String (from value)",
            ],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "for-expanded call",
            calls: r#"for item in ["first", "second"] {
  outer {
    count = "not-an-int"
  }
}
"#,
            outer_arguments: "arguments {\n  count: Int\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'count': expected Int, got string 'not-an-int'",
            ],
            expected_cli_line: Some(3),
            expected_lsp_range: Some(((2, 12), (2, 24))),
        },
    ];

    for case in &cases {
        let schemas = module_call_parity_schemas();
        let fixture = module_call_parity_fixture(case);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "calls.crn");
        let expected =
            canonical_error_multiset(case.expected.iter().map(|message| message.to_string()));

        assert_eq!(
            cli_located_error_multiset(&cli),
            expected,
            "{}: CLI must expose the exact canonical blocking findings; CLI={cli:#?}",
            case.name,
        );
        assert_eq!(
            lsp_error_multiset(&lsp),
            expected,
            "{}: LSP must expose the exact canonical ERROR findings; LSP={lsp:#?}",
            case.name,
        );
        assert_eq!(
            cli_located_error_multiset(&cli),
            lsp_error_multiset(&lsp),
            "{}: CLI and LSP must preserve the same message multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
            case.name,
        );
        if let Some(line) = case.expected_cli_line {
            assert!(
                cli.iter()
                    .any(|diagnostic| diagnostic.contains(&format!("calls.crn at line {line}\n"))),
                "{}: CLI must retain the authored argument-value line; CLI={cli:#?}",
                case.name,
            );
        }
        if let Some((start, end)) = case.expected_lsp_range {
            assert_eq!(
                lsp[0].range,
                tower_lsp::lsp_types::Range::new(
                    tower_lsp::lsp_types::Position::new(start.0, start.1),
                    tower_lsp::lsp_types::Position::new(end.0, end.1),
                ),
                "{}: LSP must underline the authored argument value",
                case.name,
            );
        }
    }
}

#[test]
fn nested_module_bare_binding_reference_mismatch_has_exact_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("nested binding-reference parity fixture");
    let outer = fixture.path().join("modules/outer");
    let producer = fixture.path().join("modules/producer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&producer).expect("create producer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        "let outer = use { source = \"./modules/outer\" }\n",
    )
    .expect("write root import");
    std::fs::write(fixture.path().join("calls.crn"), "outer {}\n").expect("write root call");
    std::fs::write(
        outer.join("imports.crn"),
        r#"let producer = use { source = "../producer" }
let inner = use { source = "../inner" }
"#,
    )
    .expect("write nested imports");
    let nested_call = r#"let produced = producer { value = 7 }
let forwarded = produced.forwarded
inner { value = forwarded }
"#;
    std::fs::write(outer.join("calls.crn"), nested_call).expect("write nested call");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes { loaded: Bool = true }\n",
    )
    .expect("write outer attribute");
    std::fs::write(producer.join("arguments.crn"), "arguments { value: Int }\n")
        .expect("write producer arguments");
    std::fs::write(
        producer.join("attributes.crn"),
        "attributes { forwarded = value }\n",
    )
    .expect("write producer attribute");
    std::fs::write(inner.join("arguments.crn"), "arguments { value: String }\n")
        .expect("write inner arguments");

    let schemas = SchemaRegistry::new();
    assert_module_resolution_expands(&outer, &schemas);
    let cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(schemas),
        &fixture,
        "modules/outer/calls.crn",
    );
    let message = "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from produced.forwarded)";
    let expected = canonical_error_multiset([message.to_string()]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing nested binding mismatch: {lsp:#?}"));
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, "inner { value = ".len() as u32),
            tower_lsp::lsp_types::Position::new(
                2,
                ("inner { value = ".len() + "forwarded".len()) as u32,
            ),
        ),
        "the typed binding mismatch must underline the bare value use",
    );
}

#[test]
fn for_expanded_root_bare_binding_reference_mismatch_has_exact_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("expanded binding-reference parity fixture");
    let producer = fixture.path().join("modules/producer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&producer).expect("create producer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"let producer = use { source = "./modules/producer" }
let inner = use { source = "./modules/inner" }
"#,
    )
    .expect("write root imports");
    std::fs::write(producer.join("arguments.crn"), "arguments { value: Int }\n")
        .expect("write producer arguments");
    std::fs::write(
        producer.join("attributes.crn"),
        "attributes { forwarded = value }\n",
    )
    .expect("write producer attribute");
    let calls = r#"let produced = producer { value = 7 }
let forwarded = produced.forwarded
for item in ["one"] {
  inner { value = forwarded }
}
"#;
    std::fs::write(fixture.path().join("calls.crn"), calls).expect("write expanded call");
    std::fs::write(inner.join("arguments.crn"), "arguments { value: String }\n")
        .expect("write inner arguments");

    let schemas = SchemaRegistry::new();
    assert_module_resolution_expands(fixture.path(), &schemas);
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "calls.crn");
    let message = "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from produced.forwarded)";
    let expected = canonical_error_multiset([message.to_string()]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing expanded binding mismatch: {lsp:#?}"));
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(3, "  inner { value = ".len() as u32),
            tower_lsp::lsp_types::Position::new(
                3,
                ("  inner { value = ".len() + "forwarded".len()) as u32,
            ),
        ),
        "the expanded typed mismatch must keep the authored bare-use range",
    );
}

#[test]
fn mixed_module_call_and_export_reference_errors_are_each_reported_once() {
    let fixture = tempfile::tempdir().expect("mixed reference-error fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("values.crn"),
        r#"provider test {}
let source = test.r.Source {
  name = "source"
  count = 7
}
"#,
    )
    .expect("write sibling values");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }

inner { value = source.count }

exports {
  bad: Int = source.name
}
"#,
    )
    .expect("write root call and export");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { value: String }\n",
    )
    .expect("write module signature");

    let schemas = module_call_parity_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let call_error = "argument 'value': cannot assign Int to 'value': expected String, got Int (from source.count)";
    let export_error = "export 'bad': type mismatch for 'source.name': expected Int, got String";

    assert_eq!(
        cli_messages_count(&cli, call_error),
        1,
        "CLI must not repeat the located module-call error in the residual block: {cli:#?}",
    );
    assert_eq!(
        cli_messages_count(&cli, export_error),
        1,
        "CLI must retain the uncovered export error exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_messages_count(&lsp, call_error),
        1,
        "LSP must retain the located module-call error exactly once: {lsp:#?}",
    );
    assert_eq!(
        lsp_messages_count(&lsp, export_error),
        1,
        "LSP must retain the uncovered export error exactly once: {lsp:#?}",
    );
}

#[test]
fn unannotated_forwarded_module_output_mismatch_has_cli_lsp_error_parity() {
    let fixture = tempfile::tempdir().expect("forwarded-output parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let outer = use { source = "./outer" }

let outer_instance = outer {}
"#,
    )
    .expect("write root call");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = "../inner" }

let inner_instance = inner { value = "forwarded" }

attributes {
  bad: Int = inner_instance.forwarded
}
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        r#"arguments { value: String }

attributes {
  forwarded = value
}
"#,
    )
    .expect("write inner module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_mismatches = cli
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'bad': type mismatch")
                && diagnostic.contains("expected Int")
                && diagnostic.contains("got String")
        })
        .collect::<Vec<_>>();
    let lsp_mismatches = lsp
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'bad': type mismatch")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        cli_mismatches.len(),
        1,
        "CLI must retain the expanded output-declaration failure: {cli:#?}",
    );
    assert_eq!(
        lsp_mismatches.len(),
        1,
        "LSP must retain the same expanded output-declaration failure: {lsp:#?}",
    );
    assert_eq!(
        lsp_mismatches[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    let cli_message = cli_located_message_multiset(&cli)
        .into_keys()
        .find(|message| message.contains("attribute 'bad': type mismatch"))
        .expect("canonical CLI mismatch message");
    assert_eq!(
        cli_message, lsp_mismatches[0].message,
        "CLI and LSP must render the core finding identically",
    );
}

#[test]
fn uncalled_imported_module_internal_call_has_cli_lsp_error_parity() {
    let fixture = tempfile::tempdir().expect("uncalled-module parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    let source = fixture.path().join("source");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::create_dir_all(&source).expect("create source module");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let unused_outer = use { source = \"./outer\" }\n",
    )
    .expect("write uncalled root import");
    let outer_source = r#"
let inner = use { source = "../inner" }
let source = use { source = "../source" }

let source_instance = source {}
inner { value = source_instance.value }
attributes { loaded: Bool = true }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("write outer module");
    std::fs::write(inner.join("main.crn"), "arguments { value: String }\n")
        .expect("write inner module");
    std::fs::write(source.join("main.crn"), "attributes { value: Int = 1 }\n")
        .expect("write source module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "outer/main.crn",
    );
    let lsp_errors = lsp_error_multiset(&lsp);
    let expected = CanonicalDiagnostic {
        message: "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from source_instance.value)".to_string(),
        severity: "ERROR",
    };

    assert_eq!(
        lsp_errors.get(&expected),
        Some(&1),
        "the authored internal call must be reported once: {lsp:#?}",
    );
    assert_eq!(
        cli_located_error_multiset(&cli).get(&expected),
        Some(&1),
        "CLI must walk an imported-but-uncalled module and render the same core finding: {cli:#?}",
    );
}

#[test]
fn nested_call_below_uncalled_import_routes_to_the_root_call_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("nested uncalled-import parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    let leaf = fixture.path().join("leaf");
    for directory in [&outer, &inner, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = r#"let outer = use { source = "./outer" }

outer {}
"#;
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write root call fixture");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = "../inner" }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        r#"let leaf = use { source = "../leaf" }

leaf { bogus = 1 }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write inner module");
    std::fs::write(leaf.join("main.crn"), "arguments { expected: Int = 0 }\n")
        .expect("write leaf module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let expected =
        canonical_error_multiset([String::from("Unknown argument 'bogus' for module 'leaf'")]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must retain the nested call failure exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must route the same nested failure to the root call: {lsp:#?}",
    );
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, 0),
            tower_lsp::lsp_types::Position::new(2, "outer {}".len() as u32),
        ),
        "the nested finding must underline the authored root call",
    );
}

#[test]
fn nested_call_below_root_uncalled_import_routes_to_the_use_statement() {
    let fixture = tempfile::tempdir().expect("root uncalled-import parity fixture");
    let outer = fixture.path().join("outer");
    let leaf = fixture.path().join("leaf");
    for directory in [&outer, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = "let outer = use { source = \"./outer\" }\n";
    std::fs::write(fixture.path().join("main.crn"), root_source)
        .expect("write root import fixture");
    std::fs::write(
        outer.join("main.crn"),
        r#"let leaf = use { source = "../leaf" }

leaf { bogus = 1 }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(leaf.join("main.crn"), "arguments { expected: Int = 0 }\n")
        .expect("write leaf module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let expected =
        canonical_error_multiset([String::from("Unknown argument 'bogus' for module 'leaf'")]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(
                0,
                root_source.trim_end_matches('\n').chars().count() as u32,
            ),
        ),
        "an uncalled root import must retain an authored editor anchor",
    );
}

#[test]
fn uncalled_import_type_resolution_error_has_cli_lsp_parity_and_declaration_range() {
    let fixture = tempfile::tempdir().expect("uncalled type-resolution parity fixture");
    let invalid = fixture.path().join("invalid");
    std::fs::create_dir_all(&invalid).expect("create invalid module directory");
    let root_source = "let invalid = use { source = \"./invalid\" }\n";
    let declaration_source = "arguments {\n  x: NoSuchType\n}\n";
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write uncalled import");
    std::fs::write(invalid.join("arguments.crn"), declaration_source)
        .expect("write invalid argument declaration");

    let schemas = module_call_parity_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let engine = engine_with_schemas(schemas);
    let root_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let declaration_lsp = lsp_diagnostics(&engine, &fixture, "invalid/arguments.crn");
    let expected = canonical_error_multiset([String::from(
        "argument 'x': unknown custom type 'NoSuchType'",
    )]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(
        lsp_error_multiset(&root_lsp),
        expected,
        "root LSP must route the declaration failure to the uncalled use: {root_lsp:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&declaration_lsp),
        expected,
        "module-file LSP must render the same typed failure: {declaration_lsp:#?}",
    );
    assert_eq!(
        root_lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(
                0,
                root_source.trim_end_matches('\n').chars().count() as u32,
            ),
        ),
    );
    assert_eq!(
        declaration_lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 5),
            tower_lsp::lsp_types::Position::new(1, 15),
        ),
        "the authored parameter type, not an unrelated module call, must be underlined",
    );
}

#[test]
fn standalone_module_type_resolution_keeps_cli_location_and_lsp_range() {
    let fixture = tempfile::tempdir().expect("standalone type-resolution parity fixture");
    let module = fixture.path().join("module");
    std::fs::create_dir_all(&module).expect("create standalone module");
    let declaration_source = "arguments {\n  x: NoSuchType\n}\n";
    std::fs::write(module.join("arguments.crn"), declaration_source)
        .expect("write module arguments");
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  loaded: Bool = true\n}\n",
    )
    .expect("write module attributes");

    let schemas = module_call_parity_schemas();
    let strict_context = carina_core::parser::ProviderContext {
        decryptor: None,
        custom_types: carina_core::provider::provider_custom_types_from_factories(Arc::new(
            factories_for(&schemas),
        )),
        resource_types: carina_core::parser::ProviderContext::resource_types_from_schema_registry(
            &schemas,
        ),
    };
    let directory_failure =
        match carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
            &module,
            &strict_context,
            &HashMap::new(),
        ) {
            Ok(_result) => panic!("the enriched directory parse must reject NoSuchType"),
            Err(failure) => failure,
        };
    assert_eq!(
        directory_failure.source_file(),
        Some(module.join("arguments.crn").as_path()),
    );
    assert!(directory_failure.parse_error().is_some_and(|error| {
        matches!(
            error,
            carina_core::parser::ParseError::UnresolvedDeclarationType {
                token,
                line,
                message,
            } if *line == 2
                && token.name() == "NoSuchType"
                && message == "unknown custom type 'NoSuchType'"
        )
    }));

    let cli = cli_diagnostics_at(factories_for(&schemas), &module);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(schemas),
        &fixture,
        "module/arguments.crn",
    );
    let message = "argument 'x': unknown custom type 'NoSuchType'";
    let expected = canonical_error_multiset([message.to_string()]);
    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must replace the parser backstop with one contextual error: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must replace the parser backstop with the same contextual ERROR: {lsp:#?}",
    );
    let cli_diagnostic = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(message))
        .unwrap_or_else(|| panic!("missing CLI type-resolution diagnostic: {cli:#?}"));
    let lsp_diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing LSP type-resolution diagnostic: {lsp:#?}"));
    assert_eq!(
        lsp_diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );

    assert!(
        cli_diagnostic.contains("module/arguments.crn at line 2\n"),
        "CLI must retain the declaration file and one-based line: {cli:#?}",
    );
    assert_eq!(
        lsp_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 5),
            tower_lsp::lsp_types::Position::new(1, 15),
        ),
        "LSP must underline the same line-two declaration named by the CLI",
    );
}

#[test]
fn nested_constraint_keeps_cli_location_and_lsp_owner_range() {
    let fixture = tempfile::tempdir().expect("nested located-constraint parity fixture");
    let outer = fixture.path().join("outer");
    let checked = fixture.path().join("checked");
    for directory in [&outer, &checked] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = r#"let outer = use { source = "./outer" }

let instance = outer {}
"#;
    let outer_source = r#"let checked = use { source = "../checked" }

checked {
  name = ""
}

attributes { loaded: Bool = true }
"#;
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write root call");
    std::fs::write(outer.join("main.crn"), outer_source).expect("write nested call");
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) > 0
      error_message = "nested name must not be empty"
    }
  }
}
"#,
    )
    .expect("write checked arguments");
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .expect("write checked attributes");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let engine = engine_with_schemas(SchemaRegistry::new());
    let root_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let nested_lsp = lsp_diagnostics(&engine, &fixture, "outer/main.crn");
    let message = "anonymous call to module 'checked': argument 'name': nested name must not be empty (got '')";
    let cli_diagnostic = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(message))
        .unwrap_or_else(|| panic!("missing CLI nested constraint: {cli:#?}"));
    let root_diagnostic = root_lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing routed root LSP constraint: {root_lsp:#?}"));
    let nested_diagnostic = nested_lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing nested-file LSP constraint: {nested_lsp:#?}"));

    assert!(
        cli_diagnostic.contains("outer/main.crn at line 4\n"),
        "CLI must retain the immediate nested argument location: {cli:#?}",
    );
    assert_eq!(
        nested_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(3, 9),
            tower_lsp::lsp_types::Position::new(3, 11),
        ),
        "the CLI line-four anchor and nested LSP range must name the same argument",
    );
    assert_eq!(
        root_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, 15),
            tower_lsp::lsp_types::Position::new(2, 23),
        ),
        "the typed finding must retain its root owner for editor routing",
    );
}

#[test]
fn constraint_and_later_unknown_argument_are_collected_by_cli_and_lsp() {
    let fixture = tempfile::tempdir().expect("collect-all module-call parity fixture");
    let checked = fixture.path().join("checked");
    let other = fixture.path().join("other");
    let leaf = fixture.path().join("leaf");
    for directory in [&checked, &other, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"let checked = use { source = "./checked" }
let other = use { source = "./other" }
"#,
    )
    .expect("write root imports");
    let calls_source = r#"checked {
  name = ""
}

other {
}
"#;
    std::fs::write(fixture.path().join("calls.crn"), calls_source).expect("write root calls");
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}
"#,
    )
    .expect("write checked arguments");
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .expect("write checked attributes");
    std::fs::write(
        other.join("arguments.crn"),
        "attributes {\n  loaded: Bool = true\n}\n",
    )
    .expect("write other attributes");
    std::fs::write(
        other.join("imports.crn"),
        "let leaf = use { source = '../leaf' }\n",
    )
    .expect("write nested import");
    std::fs::write(
        other.join("calls.crn"),
        "leaf {\n  known = true\n  bogus = true\n}\n",
    )
    .expect("write nested invalid call");
    std::fs::write(
        leaf.join("arguments.crn"),
        "arguments {\n  known: Bool\n}\n",
    )
    .expect("write leaf arguments");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "calls.crn",
    );
    let constraint =
        "anonymous call to module 'checked': argument 'name': name must not be empty (got '')";
    let unknown = "Unknown argument 'bogus' for module 'leaf'";
    let expected = canonical_error_multiset([constraint.to_string(), unknown.to_string()]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must report the constraint and later resolver failure together: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must report the same two typed findings: {lsp:#?}",
    );
    let cli_unknown = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(unknown))
        .unwrap_or_else(|| panic!("missing CLI nested unknown argument: {cli:#?}"));
    assert!(
        cli_unknown.contains("other/calls.crn at line 3\n"),
        "CLI must retain the nested argument anchor: {cli:#?}",
    );
    for (message, range) in [
        (
            constraint,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(1, 9),
                tower_lsp::lsp_types::Position::new(1, 11),
            ),
        ),
        (
            unknown,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(4, 0),
                tower_lsp::lsp_types::Position::new(5, 1),
            ),
        ),
    ] {
        let diagnostic = lsp
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("missing LSP diagnostic {message:?}: {lsp:#?}"));
        assert_eq!(diagnostic.range, range);
    }
}

#[test]
fn nested_constraint_failure_routes_each_instance_message_to_its_root_call() {
    let fixture = tempfile::tempdir().expect("nested repeated-constraint parity fixture");
    let outer = fixture.path().join("outer");
    let checked = fixture.path().join("checked");
    for directory in [&outer, &checked] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let first_line = "let a = outer { name = \"first-too-long\" }";
    let second_line = "let b = outer { name = \"second-too-long\" }";
    let root_source =
        format!("let outer = use {{ source = \"./outer\" }}\n\n{first_line}\n{second_line}\n");
    std::fs::write(fixture.path().join("main.crn"), &root_source)
        .expect("write repeated root calls");
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments { name: String }

let checked = use { source = "../checked" }
checked { name = name }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(
        checked.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) <= 3
      error_message = "name is too long"
    }
  }
}
"#,
    )
    .expect("write checked module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let first_message = "anonymous call to module 'checked': argument 'name': name is too long (got 'first-too-long')";
    let second_message = "anonymous call to module 'checked': argument 'name': name is too long (got 'second-too-long')";
    let expected =
        canonical_error_multiset([first_message.to_string(), second_message.to_string()]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must retain both concrete constraint outcomes: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must retain both concrete constraint outcomes: {lsp:#?}",
    );
    for (message, line, source_line) in [
        (first_message, 2, first_line),
        (second_message, 3, second_line),
    ] {
        let diagnostic = lsp
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("missing per-instance diagnostic {message:?}: {lsp:#?}"));
        assert_eq!(
            diagnostic.severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        );
        assert_eq!(
            diagnostic.range,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(line, "let a = ".len() as u32),
                tower_lsp::lsp_types::Position::new(line, source_line.len() as u32),
            ),
            "each concrete constraint message must underline its own root call",
        );
    }
}

#[test]
fn postfix_wrong_shape_access_has_cli_lsp_parity_for_local_and_wait_bindings() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "values.crn",
            r#"let r0 = mock.test.resource { name = "z" }
let q = mock.test.resource {
  name = "q-z"
  comment = "comment"
}
let forwarded = mock.test.resource { name = r0.identifier }
let qw = wait q {
  until = q.name == "q-z"
  timeout = 1min
}
"#,
        ),
        (
            "main.crn",
            r#"let literal = mock.test.resource { name = q.name[0] }
let provider_populated = mock.test.resource { name = q.identifier[0] }
let waited = mock.test.resource { name = qw.name["key"] }
let forwarded_value = mock.test.resource { name = forwarded.name[1] }
let optional = mock.test.resource { name = q.comment[2] }
let valid = mock.test.resource { name = q.comment }
"#,
        ),
    ]);
    let schemas = carina_provider_mock::builtin_schemas().into_iter().fold(
        SchemaRegistry::new(),
        |mut registry, schema| {
            registry.insert("mock", schema);
            registry
        },
    );

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let mut cli_access_errors = cli
        .iter()
        .filter_map(|message| {
            message
                .find("cannot apply access")
                .map(|start| message[start..].to_string())
        })
        .collect::<Vec<_>>();
    let mut lsp_access_errors = lsp
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("cannot apply access"))
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    cli_access_errors.sort();
    lsp_access_errors.sort();

    assert_eq!(
        cli_access_errors, lsp_access_errors,
        "CLI validate and LSP must report the same reference-path access errors; \
         CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert_eq!(
        cli_access_errors,
        [
            "cannot apply access ['key'] to String",
            "cannot apply access [0] to String",
            "cannot apply access [0] to String",
            "cannot apply access [1] to String",
            "cannot apply access [2] to String",
        ],
        "every statically String-valued path must be rejected while the no-suffix control stays valid",
    );
}

#[test]
fn cli_module_call_uses_argument_type_declared_in_sibling_file() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("arguments.crn"),
        "arguments {\n  count: Int\n}\n",
    )
    .expect("write directory arguments");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = './modules/inner' }

inner {
  name = count
}
"#,
    )
    .expect("write module call");
    std::fs::write(inner.join("main.crn"), "arguments {\n  name: String\n}\n")
        .expect("write inner module");

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains(
                "module call 'inner (anonymous call)': argument 'name': cannot assign Int to 'name': expected String, got Int (from count)",
            ) && diagnostic.contains("main.crn")
                && diagnostic.contains("line 4")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mismatches.len(),
        1,
        "BindingIndex must report the merged directory argument type once: {diagnostics:#?}"
    );
}

#[test]
fn nested_string_argument_to_literal_union_is_accepted_consistently() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("modules/outer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    let root = r#"let outer = use { source = './modules/outer' }

outer {
  outer_env = 'dev'
}
"#;
    std::fs::write(fixture.path().join("main.crn"), root).expect("write root");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = '../inner' }

arguments {
  outer_env: String
}

inner {
  env = outer_env
}
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments {\n  env: 'dev' | 'prod'\n}\n",
    )
    .expect("write inner module");

    // #2648 lifts the literal-only union to plain String, so a declared String
    // source remains assignable in both CLI and LSP validation.
    let cli = cli_diagnostics(Vec::new(), &fixture);
    assert!(
        cli.iter()
            .all(|message| !message.contains("Invalid argument type")),
        "CLI validation must use schema assignability policy: {cli:#?}"
    );
    let outer_path = outer.join("main.crn");
    let outer_text = std::fs::read_to_string(&outer_path).expect("read outer module");
    let outer_doc = Document::new_with_path(
        outer_text,
        outer_path.clone(),
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    let lsp = engine_with_schemas(SchemaRegistry::new()).analyze(&outer_doc, Some(&outer_path));
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("Invalid argument type")),
        "LSP and CLI must agree on the String boundary: {lsp:#?}"
    );
}

#[test]
fn nested_int_argument_to_literal_union_is_rejected_by_cli_and_lsp() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("modules/outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("create nested modules");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let outer = use { source = './modules/outer' }\nouter {\n  n = 1\n}\n",
    )
    .expect("write root");
    std::fs::write(
        outer.join("main.crn"),
        "let inner = use { source = './inner' }\narguments {\n  n: Int\n}\ninner {\n  env = n\n}\n",
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments {\n  env: 'dev' | 'prod'\n}\n",
    )
    .expect("write inner module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    assert!(
        cli.iter().any(|message| {
            message.contains("Type mismatch: expected 'dev' | 'prod', got Int (from n)")
        }),
        "CLI BindingIndex must reject Int forwarded to a literal-string union: {cli:#?}"
    );

    let outer_path = outer.join("main.crn");
    let outer_text = std::fs::read_to_string(&outer_path).expect("read outer module");
    let outer_doc = Document::new_with_path(
        outer_text,
        outer_path.clone(),
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    let lsp = engine_with_schemas(SchemaRegistry::new()).analyze(&outer_doc, Some(&outer_path));
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("Type mismatch: expected 'dev' | 'prod', got Int (from n)")
        }),
        "LSP BindingIndex must reject the same nested forwarding: {lsp:#?}"
    );
}

#[test]
fn resource_handle_forwarding_is_checked_in_root_module_and_lsp() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider mock {}
let r = mock.test.resource { name = 'r' }
let outer = use { source = './outer' }
outer { a = r }
"#,
    )
    .expect("root configuration");
    let outer_source = r#"let inner = use { source = './inner' }
arguments { a: mock.test.resource }
inner { x = a }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("outer module");
    std::fs::write(inner.join("main.crn"), "arguments { x: Int }\n").expect("inner module");

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let expected = "Type mismatch: expected Int, got mock.test.resource (from a)";

    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&root_cli, expected),
        "root validation must reject Ref -> Int forwarding: {root_cli:#?}"
    );
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    assert!(
        module_cli.iter().any(|message| {
            message.contains("Type mismatch: expected Int, got mock.test.resource (from a)")
                && message.contains("main.crn")
                && message.contains("line 3")
        }),
        "validating the module directory must reject Ref -> Int forwarding: {module_cli:#?}"
    );
    let outer_path = outer.join("main.crn");
    let lsp = engine_with_schemas(schemas.clone()).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        ),
        Some(&outer_path),
    );
    assert!(
        lsp_messages_contain(&lsp, expected),
        "LSP must reject Ref -> Int forwarding: {lsp:#?}"
    );

    std::fs::write(inner.join("main.crn"), "arguments { x: String }\n")
        .expect("String sink module");
    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let lsp = engine_with_schemas(schemas).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        ),
        Some(&outer_path),
    );
    assert!(
        root_cli
            .iter()
            .chain(&module_cli)
            .all(|message| { !message.contains("cannot assign mock.test.resource to 'x'") })
            && lsp.iter().all(|diagnostic| {
                !diagnostic
                    .message
                    .contains("cannot assign mock.test.resource to 'x'")
            }),
        "Ref -> String must preserve id-string compatibility; root={root_cli:#?}, module={module_cli:#?}, lsp={lsp:#?}"
    );
}

#[test]
fn nested_module_accepts_resource_typed_argument_through_composition_signature() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider mock {}
let resource = mock.test.resource { name = 'r' }
let outer = use { source = './outer' }
outer { resource = resource }
"#,
    )
    .expect("root configuration");
    let outer_source = r#"let inner = use { source = './inner' }
arguments { resource: mock.test.resource }
inner { resource = resource }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments { resource: mock.test.resource }\n",
    )
    .expect("inner module");

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let outer_path = outer.join("main.crn");
    let lsp = engine_with_schemas(schemas).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        ),
        Some(&outer_path),
    );

    assert!(
        root_cli.is_empty() && module_cli.is_empty() && lsp.is_empty(),
        "the nested composition signature must accept the resource handle; root={root_cli:#?}, module={module_cli:#?}, lsp={lsp:#?}",
    );
}

#[test]
fn cli_checks_reference_types_at_user_function_arguments_and_returns() {
    let fx1 = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(x: Int) { 'fixed' }
mock.test.resource { name = f(r.name) }
"#;
    let fn14 = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(): Int { r.name }
mock.test.resource { name = f() }
"#;
    let matching = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(x: String): String { x }
mock.test.resource { name = f(r.name) }
"#;
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    for (name, source) in [("fx1", fx1), ("fn14", fn14)] {
        let fixture = write_fixture(&[("main.crn", source)]);
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("function 'f'") && message.contains("expected Int")
            }),
            "{name} must reject the reference-valued function boundary: {diagnostics:#?}"
        );
    }

    let fixture = write_fixture(&[("main.crn", matching)]);
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("function 'f'")),
        "matching function argument and return types must pass: {diagnostics:#?}"
    );
}

#[test]
fn cli_checks_nested_user_function_return_types_across_declaration_order_and_files() {
    let forward_declared = write_fixture(&[(
        "main.crn",
        r#"fn f(x: Int): String { 'ok' }
let result = f(g(1))
fn g(y: Int): String { 'str' }
"#,
    )]);
    let sibling_declared = write_fixture(&[
        (
            "main.crn",
            "fn f(x: Int): String { 'ok' }\nlet result = f(g(1))\n",
        ),
        ("funcs.crn", "fn g(y: Int): String { 'str' }\n"),
    ]);

    for (label, fixture) in [
        ("forward-declared", &forward_declared),
        ("sibling-declared", &sibling_declared),
    ] {
        let diagnostics = cli_diagnostics(Vec::new(), fixture);
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("function 'f'")
                    && message.contains("expected Int")
                    && (message.contains("got String") || message.contains("got string"))
            }),
            "{label} g(): String must not satisfy f(Int): {diagnostics:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|message| !message.contains("\"str\"")),
            "evaluated nested-call contents must stay redacted: {diagnostics:#?}"
        );
    }

    let matching = write_fixture(&[
        (
            "main.crn",
            "fn f(x: Int): String { 'ok' }\nlet result = f(g(1))\n",
        ),
        ("funcs.crn", "fn g(y: Int): Int { y }\n"),
    ]);
    let diagnostics = cli_diagnostics(Vec::new(), &matching);
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("function 'f'")),
        "an Int-returning nested function must remain accepted: {diagnostics:#?}"
    );
}

#[test]
fn cli_rejects_concrete_attribute_literal_mismatch() {
    let fixture = write_fixture(&[("main.crn", "attributes {\n  y: 'dev' = 'prod'\n}\n")]);

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'y'")
                && diagnostic.contains("expected 'dev', got string 'prod'")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mismatches.len(),
        1,
        "carina validate must type-check concrete attributes values: {diagnostics:#?}"
    );
}

#[test]
fn module_validation_evaluation_error_is_reported_once_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  x: String {
    validation {
      condition     = x > 5
      error_message = "x must be greater than five"
    }
  }
}
"#,
    )
    .expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let checked = use { source = './checked' }

let instance = checked {
  x = "abc"
}
"#,
    )
    .expect("write root configuration");

    let lsp_diags = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_diags = cli_diagnostics(Vec::new(), &fixture);
    let lsp_constraint_messages = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("x must be greater than five"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    let cli_constraint_messages = cli_located_message_multiset(&cli_diags)
        .into_iter()
        .filter(|(message, _count)| message.contains("x must be greater than five"))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        lsp_constraint_messages.len(),
        1,
        "LSP must report the evaluator error once: {lsp_constraint_messages:#?}; all diagnostics: {lsp_diags:#?}"
    );
    assert_eq!(
        cli_constraint_messages.len(),
        1,
        "CLI must report the evaluator error once: {cli_constraint_messages:#?}; all diagnostics: {cli_diags:#?}"
    );
    assert_eq!(
        cli_constraint_messages.get(lsp_constraint_messages[0]),
        Some(&1),
    );
    assert_eq!(
        lsp_constraint_messages,
        [
            "module 'checked' (call 'instance'): argument 'x': x must be greater than five (got 'abc'); error evaluating constraint: cannot compare string with int"
        ]
    );
}

fn reference_valued_module_scope_fixture(module_source: &str) -> (TempDir, SchemaRegistry) {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(module.join("main.crn"), module_source).expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider test {}

let checked = use { source = './checked' }

let name_source = test.r.producer {
  name = "name"
}

let peer_source = test.r.producer {
  name = "peer"
}

let instance = checked {
  name = name_source.id
  peer = peer_source.id
}
"#,
    )
    .expect("write root configuration");

    let producer = ResourceSchema::new("r.producer")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build());
    (fixture, single_schema_map(producer))
}

fn assert_single_cli_lsp_constraint_scope_error(
    fixture: &TempDir,
    schemas: &SchemaRegistry,
    expected: &str,
) {
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(schemas), fixture);
    let lsp_matching = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains(expected))
        .collect::<Vec<_>>();
    let cli_matching = cli_diags
        .iter()
        .filter(|diagnostic| diagnostic.contains(expected))
        .collect::<Vec<_>>();

    assert_eq!(
        lsp_matching.len(),
        1,
        "LSP must report the static constraint scope error once: {lsp_diags:#?}"
    );
    assert_eq!(
        cli_matching.len(),
        1,
        "CLI validate must report the static constraint scope error once: {cli_diags:#?}"
    );
}

#[test]
fn reference_valued_require_rejects_undeclared_variable_during_validation() {
    let (fixture, schemas) = reference_valued_module_scope_fixture(
        r#"arguments {
  name: String
  peer: String
}

require nmae != peer, "names must differ"
"#,
    );

    assert_single_cli_lsp_constraint_scope_error(
        &fixture,
        &schemas,
        "unknown variable 'nmae' in constraint expression",
    );
}

#[test]
fn reference_valued_argument_validation_rejects_sibling_variable_during_validation() {
    let (fixture, schemas) = reference_valued_module_scope_fixture(
        r#"arguments {
  name: String {
    validation {
      condition     = name != peer
      error_message = "names must differ"
    }
  }
  peer: String
}
"#,
    );

    assert_single_cli_lsp_constraint_scope_error(
        &fixture,
        &schemas,
        "unknown variable 'peer' in constraint expression",
    );
}

#[test]
fn anonymous_module_constraint_hides_synthetic_instance_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
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
    .expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let checked = use { source = './checked' }

checked {
  name = ""
}
"#,
    )
    .expect("write root configuration");

    let lsp_diags = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_diags = cli_diagnostics(Vec::new(), &fixture);
    let lsp_messages = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("name must not be empty"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    let cli_messages = cli_located_message_multiset(&cli_diags);

    assert_eq!(lsp_messages.len(), 1, "LSP diagnostics: {lsp_diags:#?}");
    assert_eq!(cli_messages.len(), 1, "CLI diagnostics: {cli_diags:#?}");
    assert_eq!(cli_messages.get(lsp_messages[0]), Some(&1));
    assert_eq!(
        lsp_messages,
        ["anonymous call to module 'checked': argument 'name': name must not be empty (got '')"]
    );
}

// NOTE: case-sensitive `contains`. LSP and CLI surfaces sometimes
// differ in casing (e.g. "Type mismatch" vs "type mismatch") because
// some diagnostics originate in carina-lsp and others in carina-core.
// When an assertion needs to bind to both surfaces uniformly use
// `lsp_messages_contain_ci` / `cli_messages_contain_ci` below — a
// case-sensitive miss against one surface silently makes the assertion
// half-empty.
fn lsp_messages_contain(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> bool {
    diags.iter().any(|d| d.message.contains(substring))
}

fn cli_messages_contain(diags: &[String], substring: &str) -> bool {
    diags.iter().any(|m| m.contains(substring))
}

fn lsp_messages_count(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> usize {
    diags
        .iter()
        .filter(|d| d.message.contains(substring))
        .count()
}

fn cli_messages_count(diags: &[String], substring: &str) -> usize {
    diags.iter().filter(|m| m.contains(substring)).count()
}

// LSP and CLI diagnostic wording differs in casing — LSP says
// "Type mismatch" (capital T) while CLI says "type mismatch". A
// case-sensitive `contains` against either spelling silently misses
// the other side. Use lowercase comparisons when the assertion needs
// to bind to both surfaces uniformly.
fn lsp_messages_contain_ci(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> bool {
    let needle = substring.to_ascii_lowercase();
    diags
        .iter()
        .any(|d| d.message.to_ascii_lowercase().contains(&needle))
}

fn cli_messages_contain_ci(diags: &[String], substring: &str) -> bool {
    let needle = substring.to_ascii_lowercase();
    diags
        .iter()
        .any(|m| m.to_ascii_lowercase().contains(&needle))
}

// ============================================================================
// Test ProviderFactory — schema injection without WASM
// ============================================================================

struct TestProviderFactory {
    name: String,
    schemas: Vec<ValidatedResourceSchema>,
}

impl ProviderFactory for TestProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "Test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
    }

    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        self.schemas.clone()
    }
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "noop"
    }
    fn read(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<carina_core::provider::ReadOutcome>>
    {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn read_data_source(
        &self,
        _r: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn create(
        &self,
        _id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
        _attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn update(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<carina_core::provider::UpdateOutcome>>
    {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<()>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

// ============================================================================
// Scenario 1: Enum bare / TypeQualified / fully-qualified all pass
// ============================================================================

fn enum_schemas() -> SchemaRegistry {
    let mode = AttributeType::enum_(
        carina_core::schema::enum_identity("Mode", Some("test.r")),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    single_schema_map(
        ResourceSchema::new("r.mode_holder")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("mode", mode).build()),
    )
}

fn enum_shadow_schemas() -> SchemaRegistry {
    let event_type = AttributeType::enum_(
        carina_core::schema::enum_identity("EventType", Some("awscc.cloudfront.Distribution")),
        Some(vec![
            "ViewerRequest".to_string(),
            "OriginRequest".to_string(),
        ]),
        vec![
            ("ViewerRequest".to_string(), "viewer_request".to_string()),
            ("OriginRequest".to_string(), "origin_request".to_string()),
        ],
        None,
        None,
    );
    let behavior = AttributeType::list(AttributeType::struct_(
        "Behavior",
        vec![StructField::optional("event_type", event_type.clone()).build()],
    ));
    let flow_log_resource_type = AttributeType::enum_(
        carina_core::schema::enum_identity("ResourceType", Some("awscc.ec2.FlowLog")),
        Some(vec![
            "NetworkInterface".to_string(),
            "Subnet".to_string(),
            "VPC".to_string(),
        ]),
        vec![
            (
                "NetworkInterface".to_string(),
                "network_interface".to_string(),
            ),
            ("Subnet".to_string(), "subnet".to_string()),
            ("VPC".to_string(), "vpc".to_string()),
        ],
        None,
        None,
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("test.Source")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
            .validate()
            .expect("source schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("cloudfront.Distribution")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("event_type", event_type.clone()).build())
            .attribute(
                AttributeSchema::optional("behaviors", behavior)
                    .with_block_name("behavior")
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("event_types", AttributeType::list(event_type.clone()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("event_type_map", AttributeType::map(event_type.clone()))
                    .build(),
            )
            .validate()
            .expect("distribution schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("cloudfront.RequiredDistribution")
            .attribute(AttributeSchema::required("event_type", event_type).build())
            .validate()
            .expect("required distribution schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("ec2.FlowLog")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("resource_type", flow_log_resource_type).build())
            .validate()
            .expect("flow-log schema fixture must be valid"),
    );
    schemas
}

fn bare_enum_shadow_schemas() -> SchemaRegistry {
    let mode = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Mode"),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    single_schema_map(
        ResourceSchema::new("r.mode_holder")
            .attribute(AttributeSchema::optional("mode", mode).build()),
    )
}

const ENUM_SHADOW_MESSAGE: &str = "`viewer_request` was interpreted as the `let` binding; if you meant the enum `EventType` value, write `awscc.cloudfront.Distribution.EventType.viewer_request`";

fn enum_shadow_fixture(use_site: &str) -> TempDir {
    write_fixture(&[
        (
            "bindings.crn",
            r#"let viewer_request = awscc.test.Source {
  name = "shadow"
}
"#,
        ),
        (
            "main.crn",
            &format!("awscc.cloudfront.Distribution {{\n  name = \"distribution\"\n{use_site}}}\n"),
        ),
    ])
}

fn enum_shadow_module_argument_fixture() -> TempDir {
    let fixture = tempfile::tempdir().expect("module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.test.Source { name = \"shadow\" }\n",
    )
    .expect("write binding fixture");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_type: awscc.cloudfront.Distribution.EventType\n}\n",
    )
    .expect("write module signature");
    fixture
}

#[test]
fn enum_member_shadowed_by_sibling_resource_binding_reports_typed_error_in_every_resource_position()
{
    for (position, use_site) in [
        ("attribute", Some("  event_type = viewer_request\n")),
        (
            "nested block",
            Some("  behavior {\n    event_type = viewer_request\n  }\n"),
        ),
        ("list element", Some("  event_types = [viewer_request]\n")),
        (
            "map element",
            Some("  event_type_map = { default = viewer_request }\n"),
        ),
        ("module argument", None),
    ] {
        let schemas = enum_shadow_schemas();
        let fixture =
            use_site.map_or_else(enum_shadow_module_argument_fixture, enum_shadow_fixture);
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE)),
            "{position} must report the typed enum-shadow diagnostic, got: {diagnostics:#?}",
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.contains("write 'viewer_request'")),
            "the diagnostic must not advertise a quoted-string escape rejected by strict enum validation: {diagnostics:#?}",
        );
        let matching_lsp = lsp
            .iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .collect::<Vec<_>>();
        assert_eq!(
            matching_lsp.len(),
            1,
            "{position} must report the same typed enum-shadow diagnostic in the LSP, got: {lsp:#?}",
        );
        assert_eq!(
            matching_lsp[0].severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
            "{position} blocks CLI validation and must therefore be an LSP error",
        );
    }
}

#[test]
fn structural_binding_without_matching_enum_member_keeps_existing_guidance() {
    let schemas = enum_shadow_schemas();
    for (attribute, declaration) in [
        (
            "event_type",
            "let not_a_member = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "name",
            "let not_a_member = awscc.test.Source { name = \"shadow\" }\n",
        ),
    ] {
        let fixture = write_fixture(&[
            ("bindings.crn", declaration),
            (
                "main.crn",
                &format!(
                    "awscc.cloudfront.Distribution {{\n  name = \"distribution\"\n  {attribute} = not_a_member\n}}\n"
                ),
            ),
        ]);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp_message = lsp.iter().find(|diagnostic| {
            diagnostic.message.contains(
                "'not_a_member' is a resource, not a value; reference one of its attributes",
            )
        });
        let cli_message = diagnostics.iter().find(|diagnostic| {
            diagnostic.contains(
                "'not_a_member' is a resource, not a value; reference one of its attributes",
            )
        });
        assert!(
            lsp_message.is_some() && cli_message.is_some(),
            "{attribute} must retain CLI/LSP structural-binding guidance; CLI={diagnostics:#?}; LSP={lsp:#?}",
        );
        assert!(
            cli_message
                .expect("checked above")
                .contains(&lsp_message.expect("checked above").message),
            "{attribute} guidance must render identically; CLI={diagnostics:#?}; LSP={lsp:#?}",
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.contains("`not_a_member` was interpreted")),
            "a non-member spelling must not become an enum-shadow error: {diagnostics:#?}",
        );
    }
}

#[test]
fn pending_structural_enum_use_suppresses_secondary_missing_required_attribute() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
awscc.cloudfront.RequiredDistribution {
  event_type = viewer_request
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(
        cli.iter()
            .filter(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE))
            .count(),
        1,
        "CLI must report the authored structural collision once: {cli:#?}",
    );
    assert_eq!(
        lsp.iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .count(),
        1,
        "LSP must report the authored structural collision once: {lsp:#?}",
    );
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.to_ascii_lowercase().contains("required")
                && !diagnostic.message.to_ascii_lowercase().contains("missing")
        }),
        "the typed pending slot must suppress the missing-attribute error: {lsp:#?}",
    );
}

#[test]
fn non_enum_value_binding_keeps_existing_reference_type_check() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let source = awscc.test.Source {
  name = "shadow"
  count = 1
}
let forwarded = source.count

awscc.cloudfront.Distribution {
  name = forwarded
}
"#,
    )]);

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli.iter().any(|diagnostic| {
            diagnostic.contains("Type mismatch")
                && diagnostic.contains("expected String")
                && diagnostic.contains("got Int")
        }),
        "CLI must retain the forwarded binding's reference type check: {cli:#?}",
    );
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("expected String")
                && diagnostic.message.contains("got Int")
        }),
        "LSP must retain the same forwarded binding type check: {lsp:#?}",
    );
}

#[test]
fn enum_member_shadowed_by_resource_binding_reports_cli_lsp_parity_and_quick_fix() {
    let schemas = enum_shadow_schemas();
    let fixture = enum_shadow_fixture("  event_type = viewer_request\n");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");

    let cli_message = cli
        .iter()
        .find(|message| message.contains(ENUM_SHADOW_MESSAGE))
        .expect("CLI enum-shadow diagnostic");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message.contains(ENUM_SHADOW_MESSAGE))
        .unwrap_or_else(|| panic!("LSP enum-shadow diagnostic; got {lsp:#?}"));
    assert!(
        cli_message.contains(&diagnostic.message),
        "CLI and LSP must render the same typed error; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert!(
        cli_message.contains("main.crn") && cli_message.contains("line 3"),
        "the CLI diagnostic must identify the resource owner's authored file and line: {cli_message}",
    );

    let line = "  event_type = viewer_request";
    let start = line.find("viewer_request").expect("value start") as u32;
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.end.line, 2);
    assert_eq!(diagnostic.range.start.character, start);
    assert_eq!(
        diagnostic.range.end.character,
        start + "viewer_request".len() as u32
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let action = actions
        .iter()
        .find(|action| {
            action.title == "Replace with `awscc.cloudfront.Distribution.EventType.viewer_request`"
        })
        .unwrap_or_else(|| panic!("enum-shadow quick fix; got {actions:#?}"));
    let edit = action
        .edit
        .as_ref()
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .expect("quick-fix text edit");
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(
        edit.new_text,
        "awscc.cloudfront.Distribution.EventType.viewer_request"
    );
}

#[test]
fn namespace_less_enum_shadow_keeps_the_mismatch_without_a_noop_quick_fix() {
    let schemas = bare_enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let fast = 42
test.r.mode_holder {
  mode = fast
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let mismatch = "Type mismatch: expected Mode, got Int";
    assert!(
        cli.iter().any(|message| message.contains(mismatch)),
        "CLI must keep the ordinary mismatch when no distinct enum spelling exists: {cli:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == mismatch)
        .unwrap_or_else(|| panic!("LSP must keep the ordinary mismatch: {lsp:#?}"));
    assert!(
        cli.iter().all(|message| !message.contains("write `fast`"))
            && lsp
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("write `fast`")),
        "a namespace-less enum must not suggest replacing a token with itself; CLI={cli:#?}; LSP={lsp:#?}",
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    assert!(
        carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic).is_empty(),
        "the ordinary mismatch must not carry an enum quick fix",
    );
}

#[test]
fn pending_nested_resource_handle_reports_the_authored_use_instead_of_internal_jargon() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested resource-handle fixture");
    let module = fixture.path().join("module");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let source = awscc.test.Source { name = "source" }
let imported = use { source = "./module" }
imported { sources = [source] }
"#,
    )
    .expect("write caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { sources: list(awscc.test.Source) }\n",
    )
    .expect("write module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected_message = "'source' is a resource, not a value; reference one of its attributes, e.g. 'source.<attribute>'";
    assert!(
        cli.iter().any(|message| {
            message.contains("'source' is a resource, not a value")
                && message.contains("main.crn")
                && message.contains("line 3")
        }),
        "the unrepresentable nested handle must point to the authored token: {cli:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        BTreeMap::from([(expected_message.to_string(), 1)]),
        "the CLI must emit the authored pending-use finding exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        cli_located_message_multiset(&cli),
        "CLI and LSP pending-use diagnostics must have the same messages and multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert!(
        cli.iter()
            .all(|message| !message.contains("unconsumed pending value slot")),
        "internal pending-slot terminology must not reach users: {cli:#?}",
    );
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("unconsumed pending value slot")),
        "internal pending-slot terminology must not reach LSP users: {lsp:#?}",
    );
}

#[test]
fn issue_2978_canonical_api_spelling_has_cli_lsp_parity_and_identity_derived_fix() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let VPC = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "main.crn",
            r#"awscc.ec2.FlowLog {
  name = "flow-log"
  resource_type = VPC
}
"#,
        ),
    ]);
    let expected = "`VPC` was interpreted as the `let` binding; if you meant the enum `ResourceType` value, write `awscc.ec2.FlowLog.ResourceType.vpc`";
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert!(
        cli.iter().any(|message| message.contains(expected)),
        "CLI must recognize the canonical API spelling as a member: {cli:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == expected)
        .unwrap_or_else(|| panic!("LSP canonical-spelling diagnostic; got {lsp:#?}"));

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let edit = actions
        .iter()
        .find(|action| action.title == "Replace with `awscc.ec2.FlowLog.ResourceType.vpc`")
        .and_then(|action| action.edit.as_ref())
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .unwrap_or_else(|| panic!("identity-derived canonical quick fix; got {actions:#?}"));
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(edit.new_text, "awscc.ec2.FlowLog.ResourceType.vpc");
}

#[test]
fn lsp_enum_shadow_ranges_use_exact_parser_spans_for_repeated_and_unicode_prefixed_tokens() {
    let schemas = enum_shadow_schemas();
    let source = r#"let viewer_request = awscc.test.Source { name = "shadow" }
awscc.cloudfront.Distribution {
  name = "distribution"
  event_types = [viewer_request, viewer_request]
  event_type_map = { "日本語" = viewer_request }
}
"#;
    let fixture = write_fixture(&[("main.crn", source)]);
    let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn")
        .into_iter()
        .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 3, "one finding per authored token");

    let list_line = source.lines().nth(3).unwrap();
    let list_starts = list_line
        .match_indices("viewer_request")
        .map(|(byte, _)| list_line[..byte].chars().count() as u32)
        .collect::<Vec<_>>();
    assert_eq!(list_starts.len(), 2);
    let unicode_line = source.lines().nth(4).unwrap();
    let unicode_byte = unicode_line.find("viewer_request").unwrap();
    let unicode_start = unicode_line[..unicode_byte].chars().count() as u32;

    let ranges = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.range)
        .collect::<Vec<_>>();
    for start in list_starts {
        assert!(ranges.iter().any(|range| {
            range.start.line == 3
                && range.start.character == start
                && range.end.character == start + "viewer_request".len() as u32
        }));
    }
    assert!(ranges.iter().any(|range| {
        range.start.line == 4
            && range.start.character == unicode_start
            && range.end.character == unicode_start + "viewer_request".len() as u32
    }));
}

#[test]
fn for_expansion_deduplicates_enum_shadow_diagnostics_by_authored_token() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
for name in ["first", "second"] {
  awscc.cloudfront.Distribution {
    name = name
    event_type = viewer_request
  }
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(
        cli.iter()
            .filter(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE))
            .count(),
        1,
        "expanded owners must not duplicate one authored CLI finding: {cli:#?}",
    );
    assert_eq!(
        lsp.iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .count(),
        1,
        "expanded owners must not duplicate one authored LSP finding: {lsp:#?}",
    );
}

#[test]
fn enum_typed_module_argument_reports_resource_binding_shadow() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.test.Source { name = \"shadow\" }\n",
    )
    .expect("write binding fixture");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_type: awscc.cloudfront.Distribution.EventType\n}\n",
    )
    .expect("write module signature");

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE)),
        "module argument must use the provider-owned enum declaration: {diagnostics:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
        .unwrap_or_else(|| panic!("module LSP enum-shadow diagnostic; got {lsp:#?}"));
    let line = "  event_type = viewer_request";
    let start = line.find("viewer_request").expect("module value start") as u32;
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.end.line, 2);
    assert_eq!(diagnostic.range.start.character, start);
    assert_eq!(
        diagnostic.range.end.character,
        start + "viewer_request".len() as u32,
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let edit = actions
        .iter()
        .find(|action| {
            action.title == "Replace with `awscc.cloudfront.Distribution.EventType.viewer_request`"
        })
        .and_then(|action| action.edit.as_ref())
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .unwrap_or_else(|| panic!("module enum-shadow quick fix; got {actions:#?}"));
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(
        edit.new_text,
        "awscc.cloudfront.Distribution.EventType.viewer_request",
    );
}

#[test]
fn nested_enum_typed_module_argument_descends_the_recorded_binding_path() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
let inner = use { source = "./modules/inner" }
inner {
  event_types = [viewer_request]
}
"#,
    )
    .expect("write nested caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_types: list(awscc.cloudfront.Distribution.EventType)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected = BTreeMap::from([(ENUM_SHADOW_MESSAGE.to_string(), 1)]);
    assert!(
        matches!(cli.as_slice(), [message]
            if message.contains(ENUM_SHADOW_MESSAGE)
                && message.contains("main.crn")
                && message.contains("line 4")),
        "CLI must emit only the located shared binding-use finding: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        expected,
        "LSP must suppress missing-argument and unused-binding secondaries: {lsp:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        lsp_message_multiset(&lsp),
        "CLI and LSP must preserve the same diagnostic multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
}

#[test]
fn successful_nested_binding_does_not_hide_an_invalid_module_argument_sibling() {
    let fixture = tempfile::tempdir().expect("nested module sibling fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let valid_name = "ok"
let inner = use { source = "./modules/inner" }
inner {
  names = [valid_name, 42]
}
"#,
    )
    .expect("write nested caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  names: list(String)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    assert!(
        cli.iter().any(|diagnostic| {
            diagnostic.contains("names")
                && diagnostic.contains("expected String")
                && diagnostic.to_ascii_lowercase().contains("got int")
        }),
        "CLI must retain ordinary validation for the invalid sibling: {cli:#?}",
    );
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic.message.contains("expected String")
                && diagnostic.message.to_ascii_lowercase().contains("got int")
        }),
        "LSP must not skip the whole argument after the binding use succeeds: {lsp:#?}",
    );
}

#[test]
fn nested_value_binding_shadow_replaces_the_legacy_module_argument_error() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested value-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let viewer_request = 42
let inner = use { source = "./modules/inner" }
inner {
  event_types = [viewer_request]
}
"#,
    )
    .expect("write nested value-shadow fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_types: list(awscc.cloudfront.Distribution.EventType)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected = BTreeMap::from([(ENUM_SHADOW_MESSAGE.to_string(), 1)]);
    assert!(
        matches!(cli.as_slice(), [message]
            if message.contains(ENUM_SHADOW_MESSAGE)
                && message.contains("main.crn")
                && message.contains("line 4")),
        "the CLI authority must replace its legacy argument diagnostic with one located finding: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        expected,
        "the LSP must expose exactly the same diagnostic set: {lsp:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        lsp_message_multiset(&lsp),
        "CLI and LSP must preserve the same diagnostic multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
}

#[test]
fn module_value_binding_type_mismatch_has_exact_cli_lsp_diagnostic_parity() {
    let fixture = tempfile::tempdir().expect("module value mismatch fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let count = "not-an-int"
let inner = use { source = "./modules/inner" }
inner { count = count }
"#,
    )
    .expect("write caller fixture");
    std::fs::write(module.join("main.crn"), "arguments { count: Int }\n")
        .expect("write module signature");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    assert!(
        matches!((cli.as_slice(), lsp.as_slice()), ([cli_message], [lsp_diagnostic])
            if cli_message.contains(&lsp_diagnostic.message)
                && cli_message.contains("main.crn")
                && cli_message.contains("line 3")),
        "one authored mismatch must yield the same message with a CLI source location; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "a binding-use finding that fails CLI validation must be an LSP error: {lsp:#?}",
    );
}

#[test]
fn enum_value_binding_keeps_lexical_precedence_and_preserves_the_bound_member() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let viewer_request = awscc.cloudfront.Distribution.EventType.origin_request\n",
        ),
        (
            "main.crn",
            r#"awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
        ),
    ]);

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "an assignable enum-valued binding must win over shorthand: {diagnostics:#?}",
    );
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.contains(ENUM_SHADOW_MESSAGE)
                && !diagnostic.message.contains("event_type")
        }),
        "LSP must accept the same enum-valued binding: {lsp:#?}",
    );

    let parsed = cli_validated_file(factories_for(&schemas), &fixture);
    let distribution = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("validated distribution");
    assert!(
        matches!(
            distribution.get_attr("event_type"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "OriginRequest"
        ),
        "the lexical binding must preserve the different `origin_request` member in the CLI tree: {:?}",
        distribution.get_attr("event_type"),
    );
}

#[test]
fn enum_typed_module_argument_preserves_the_bound_member_in_the_expanded_resource() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module enum-precedence fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.cloudfront.Distribution.EventType.origin_request\n",
    )
    .expect("write enum binding");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  event_type: awscc.cloudfront.Distribution.EventType
}

awscc.cloudfront.Distribution {
  name = "expanded"
  event_type = event_type
}
"#,
    )
    .expect("write module body");

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.contains(ENUM_SHADOW_MESSAGE)
                && !diagnostic.message.contains("viewer_request")
        }),
        "LSP must accept the enum-valued module argument binding: {lsp:#?}",
    );
    let parsed = cli_validated_file(factories_for(&schemas), &fixture);
    let expanded = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("expanded module distribution");
    assert!(
        matches!(
            expanded.get_attr("event_type"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "OriginRequest"
        ),
        "the module argument must preserve the different `origin_request` member after CLI expansion: {:?}",
        expanded.get_attr("event_type"),
    );
}

#[test]
fn enum_typed_argument_binding_wins_and_string_argument_reports_shadow() {
    let schemas = enum_shadow_schemas();
    let matching = write_fixture(&[(
        "main.crn",
        r#"arguments {
  viewer_request: awscc.cloudfront.Distribution.EventType
}

awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
    )]);
    let bootstrap = carina_core::config_loader::load_configuration_with_config(
        matching.path(),
        &carina_core::parser::ProviderContext::default(),
        &SchemaRegistry::new(),
    )
    .expect("schema-independent bootstrap parse");
    assert!(matches!(
        bootstrap
            .parsed
            .arguments
            .first()
            .map(|argument| &argument.static_type),
        Some(carina_core::parser::TypeExpr::DottedUnresolved(_))
    ));
    let matching_lsp =
        lsp_diagnostics(&engine_with_schemas(schemas.clone()), &matching, "main.crn");
    let matching_cli = cli_diagnostics(factories_for(&schemas), &matching);
    assert!(
        matching_cli.is_empty(),
        "an argument declared as the expected enum must keep lexical precedence: {matching_cli:#?}",
    );
    assert!(
        matching_lsp
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("viewer_request")),
        "LSP must accept the same enum-typed argument: {matching_lsp:#?}",
    );

    let mismatched = write_fixture(&[(
        "main.crn",
        r#"arguments {
  viewer_request: String
}

awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
    )]);
    let expected = "`viewer_request` was interpreted as the `argument` binding; if you meant the enum `EventType` value, write `awscc.cloudfront.Distribution.EventType.viewer_request`";
    let mismatched_lsp = lsp_diagnostics(
        &engine_with_schemas(schemas.clone()),
        &mismatched,
        "main.crn",
    );
    let mismatched_cli = cli_diagnostics(factories_for(&schemas), &mismatched);
    assert!(
        mismatched_cli
            .iter()
            .any(|diagnostic| diagnostic.contains(expected)),
        "a String argument that shadows the enum member must use the structured error: {mismatched_cli:#?}",
    );
    assert!(
        mismatched_lsp
            .iter()
            .any(|diagnostic| diagnostic.message == expected),
        "LSP must render the same argument-shadow error: {mismatched_lsp:#?}",
    );
}

#[test]
fn fully_qualified_shape_preserves_a_bound_provider_head() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let awscc = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "main.crn",
            r#"awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = awscc.cloudfront.Distribution.EventType.viewer_request
}
"#,
        ),
    ]);

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert_eq!(
        diagnostics,
        vec![
            "awscc.cloudfront.Distribution.<pending>: unknown attribute 'cloudfront' on 'awscc' in reference awscc.cloudfront"
                .to_string(),
        ],
        "the resource binding must retain precedence over enum-shaped spelling",
    );
}

#[test]
fn caller_let_forwarded_through_module_parameter_keeps_the_expanded_value_and_does_not_panic() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module forwarding fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let my_let = "caller-value"
let param = 42
let inner = use { source = "./modules/inner" }

awscc.test.Source {
  name = "root"
  count = param
}
inner {
  param = my_let
}
"#,
    )
    .expect("write caller fixture");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  param: String
}

awscc.cloudfront.Distribution {
  name = param
}
"#,
    )
    .expect("write module fixture");

    let config = carina_core::parser::ProviderContext::default();
    let mut loaded = carina_core::config_loader::load_configuration_with_config(
        fixture.path(),
        &config,
        &schemas,
    )
    .expect("load caller configuration");
    carina_core::module_resolver::resolve_modules_with_config(
        &mut loaded.parsed,
        fixture.path(),
        &config,
        &schemas,
    )
    .expect("expand module call without panicking");

    let expanded = loaded
        .parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("expanded module resource");
    assert_eq!(
        expanded.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "caller-value".into()
        ))),
        "module substitution must leave the ordinary runtime value clean",
    );

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "the module parameter must not resolve against the root's same-named Int binding: {diagnostics:#?}",
    );
}

#[test]
fn module_internal_bare_use_ignores_a_same_named_root_let() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module lexical-scope fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let shared_name = 42
let inner = use { source = "./modules/inner" }
awscc.test.Source {
  name = "root"
  count = shared_name
}
inner {}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"let shared_name = "module-value"
attributes { name = shared_name }
awscc.cloudfront.Distribution { name = shared_name }
"#,
    )
    .unwrap();

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "module-internal provenance must resolve only in the module scope: {diagnostics:#?}",
    );
}

#[test]
fn enum_bare_typequalified_fully_qualified_all_pass_parity() {
    let schemas = enum_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.mode_holder {
    name = "a"
    mode = fast
}
test.r.mode_holder {
    name = "b"
    mode = Mode.slow
}
test.r.mode_holder {
    name = "c"
    mode = test.r.Mode.fast
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);

    assert_eq!(
        lsp_messages_count(&lsp_diags, "Mode"),
        cli_messages_count(&cli_diags, "Mode"),
        "LSP and CLI Mode-mention counts must agree.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(
        lsp_messages_count(&lsp_diags, "Mode"),
        0,
        "expected no Mode diagnostics from LSP, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn enum_invalid_value_reports_diagnostic_parity() {
    let schemas = enum_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.mode_holder {
    name = "a"
    mode = "bogus"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "bogus"),
        "LSP must surface bogus value, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "bogus"),
        "CLI must surface bogus value, got {:?}",
        cli_diags,
    );
}

#[test]
fn validate_with_factories_preserves_enum_canonicalization_detail_once() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let holder = missing.r.mode_holder {
    mode = test.r.Mode.bogus
}
"#,
    )]);

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let rendered = diagnostics.join("\n");
    let leaf = "unresolved enum identifier `test.r.Mode.bogus` survived canonicalization";
    assert!(rendered.contains(leaf), "missing enum detail: {rendered}");
    assert_eq!(
        rendered.matches(leaf).count(),
        1,
        "enum detail must appear exactly once: {rendered}"
    );
}

// ============================================================================
// Scenario 2: Custom type with `to_dsl` normalization (Region-like)
// ============================================================================

fn region_schemas() -> SchemaRegistry {
    fn validate_region(v: &Value) -> Result<(), String> {
        const VALID: &[&str] = &["ap-northeast-1", "us-west-2"];
        if let Value::Concrete(ConcreteValue::String(s)) = v {
            let normalized = fixture_enum_segment_as_aws_value(s.expose_plaintext());
            if VALID.contains(&normalized.as_str()) {
                return Ok(());
            }
            return Err(format!("invalid Region '{}'", s.expose_plaintext()));
        }
        Err("expected string".to_string())
    }
    let region_custom = AttributeType::enum_(
        carina_core::schema::enum_identity("Region", Some("test")),
        None,
        vec![],
        Some(legacy_validator(validate_region)),
        Some(carina_core::schema::DslTransform::HyphenToUnderscore),
    );

    single_schema_map(
        ResourceSchema::new("r.region_holder")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("region", region_custom).build()),
    )
}

#[test]
fn region_accepts_bare_and_typequalified_forms_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let a = test.r.region_holder {
    name = "a"
    region = ap_northeast_1
}
let b = test.r.region_holder {
    name = "b"
    region = Region.us_west_2
}
let c = test.r.region_holder {
    name = "c"
    region = test.Region.ap_northeast_1
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_region =
        lsp_messages_count(&lsp_diags, "Region") + lsp_messages_count(&lsp_diags, "region");
    let cli_region =
        cli_messages_count(&cli_diags, "Region") + cli_messages_count(&cli_diags, "region");
    assert_eq!(
        lsp_region,
        cli_region,
        "LSP / CLI Region-mention parity broken.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(lsp_region, 0, "expected zero Region diagnostics");
}

#[test]
fn region_accepts_aws_string_form_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let a = test.r.region_holder {
    name = "a"
    region = "ap-northeast-1"
}
let b = test.r.region_holder {
    name = "b"
    region = "us-west-2"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_region =
        lsp_messages_count(&lsp_diags, "Region") + lsp_messages_count(&lsp_diags, "region");
    let cli_region =
        cli_messages_count(&cli_diags, "Region") + cli_messages_count(&cli_diags, "region");
    assert_eq!(
        lsp_region,
        cli_region,
        "LSP / CLI Region-string-form parity broken.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(lsp_region, 0, "expected zero Region diagnostics");
}

#[test]
fn region_rejects_unknown_value_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.region_holder {
    name = "a"
    region = mars_1
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        lsp_messages_contain(&lsp_diags, "Region") || lsp_messages_contain(&lsp_diags, "mars"),
        "LSP must reject mars_1, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "Region") || cli_messages_contain(&cli_diags, "mars"),
        "CLI must reject mars_1, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario 3: nested-Struct field type mismatch
// ============================================================================

fn nested_struct_schemas() -> SchemaRegistry {
    let inner = AttributeType::struct_(
        "Inner".to_string(),
        vec![StructField::optional("leaf", AttributeType::int()).build()],
    );
    let outer = AttributeType::struct_(
        "Outer".to_string(),
        vec![
            StructField::optional("inner", inner).build(),
            StructField::optional("label", AttributeType::string()).build(),
        ],
    );

    single_schema_map(
        ResourceSchema::new("r.nested")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("outer", outer).build()),
    )
}

#[test]
fn nested_struct_int_field_with_string_value_diagnoses_parity() {
    let schemas = nested_struct_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.nested {
    name = "a"
    outer = {
        label = "x"
        inner = { leaf = "not-an-int" }
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let needle_in = |has: &dyn Fn(&str) -> bool| has("leaf") || has("Int") || has("not-an-int");
    let lsp_hit = needle_in(&|s| lsp_messages_contain(&lsp_diags, s));
    let cli_hit = needle_in(&|s| cli_messages_contain(&cli_diags, s));
    assert!(lsp_hit, "LSP must diagnose nested int mismatch");
    assert!(
        cli_hit,
        "CLI must diagnose nested int mismatch, got {:?}",
        cli_diags
    );
}

// ============================================================================
// Scenario 4: Union with multiple member candidates
// ============================================================================

fn union_schemas() -> SchemaRegistry {
    let union = AttributeType::union(vec![AttributeType::int(), AttributeType::string()]);
    single_schema_map(
        ResourceSchema::new("r.union")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("value", union).build()),
    )
}

#[test]
fn union_accepts_either_member_parity() {
    let schemas = union_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.union {
    name = "a"
    value = 42
}
test.r.union {
    name = "b"
    value = "hello"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_mismatch = lsp_messages_count(&lsp_diags, "Type mismatch");
    let cli_mismatch = cli_messages_count(&cli_diags, "Type mismatch");
    assert_eq!(
        lsp_mismatch,
        0,
        "LSP: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(cli_mismatch, 0, "CLI: {:?}", cli_diags);
}

#[test]
fn union_rejects_non_member_type_parity() {
    let schemas = union_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.union {
    name = "a"
    value = true
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("value") || c("Bool") || c("Union");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must reject Bool"
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must reject Bool, got {:?}",
        cli_diags
    );
}

#[test]
fn tied_scalar_union_reports_every_alternative_once_with_cli_lsp_parity() {
    let hosted_zone_id = AttributeType::union(vec![
        AttributeType::enum_(
            carina_core::schema::enum_identity(
                "CloudFrontHostedZoneId",
                Some("test.route53.Record.AliasTarget"),
            ),
            Some(vec!["global".to_string()]),
            vec![],
            None,
            None,
        ),
        AttributeType::refined_string(
            Some(carina_core::schema::TypeIdentity::new(
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
        .attribute(AttributeSchema::optional("hosted_zone_id", hosted_zone_id).build());
    let attributes = HashMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("record".into())),
        ),
        (
            "hosted_zone_id".to_string(),
            Value::Concrete(ConcreteValue::String("z14grhdcwa56qt".into())),
        ),
    ]);
    let validated_schema = schema
        .clone()
        .validate()
        .expect("test schema must be valid");
    let mut core_registry = SchemaRegistry::new();
    core_registry.insert("test", validated_schema);
    let core_factories = Arc::new(factories_for(&core_registry));
    let core_resolver = carina_core::provider::provider_custom_types_from_factories(core_factories);
    let core_errors = core_registry
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
        .expect_err("both union alternatives must reject the value");
    assert_eq!(core_errors.len(), 1, "core errors: {core_errors:#?}");
    let expected = core_errors[0].to_string();
    assert!(
        expected.contains("CloudFrontHostedZoneId.global") && expected.contains("/^Z[A-Z0-9]+$/"),
        "core must retain both tied failures: {expected}"
    );

    let schemas = single_schema_map(schema);
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        (
            "main.crn",
            r#"test.route53.Record {
  name = "record"
  hosted_zone_id = "z14grhdcwa56qt"
}
"#,
        ),
    ]);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_matching = lsp
        .iter()
        .filter(|diagnostic| diagnostic.message == expected)
        .collect::<Vec<_>>();
    let cli_matching = cli
        .iter()
        .filter(|diagnostic| diagnostic.contains(&expected))
        .collect::<Vec<_>>();

    assert_eq!(
        lsp_matching.len(),
        1,
        "LSP must report the shared message once: {lsp:#?}"
    );
    assert_eq!(
        cli_matching.len(),
        1,
        "CLI validate must report the shared message once: {cli:#?}"
    );
}

// ============================================================================
// Scenario 5: ResourceRef across sibling files
// ============================================================================

fn resource_ref_schemas() -> SchemaRegistry {
    let producer = ResourceSchema::new("r.producer")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build());
    let consumer = ResourceSchema::new("r.consumer")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target_id", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        producer.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        consumer.validate().expect("schema fixture must be valid"),
    );
    schemas
}

#[test]
fn resource_ref_across_sibling_files_resolves_parity() {
    let schemas = resource_ref_schemas();
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.consumer {
    name = "c1"
    target_id = upstream.id
}
"#,
        ),
        (
            "exports.crn",
            r#"
let upstream = test.r.producer {
    name = "p1"
}
"#,
        ),
    ]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_undef = lsp_diags
        .iter()
        .filter(|d| d.message.contains("upstream") && d.message.contains("Undefined"))
        .count();
    let cli_undef = cli_diags
        .iter()
        .filter(|m| m.contains("upstream") && m.contains("Undefined"))
        .count();
    assert_eq!(
        lsp_undef,
        0,
        "LSP must resolve upstream from sibling file: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        cli_undef, 0,
        "CLI must resolve upstream from sibling file: {:?}",
        cli_diags
    );
}

#[test]
fn read_only_assignment_in_multi_file_directory_fails_validate() {
    let schemas = single_schema_map(
        ResourceSchema::new("r.bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("region", AttributeType::string()).build(),
            ),
    );
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.bucket {
    name = "configured"
}
"#,
        ),
        (
            "sibling.crn",
            r#"
test.r.bucket {
    name = "sibling"
    region = "us-west-2"
}
"#,
        ),
    ]);

    let expected = "Attribute 'region' is populated by the provider and cannot be set";
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&cli_diags, expected),
        "carina validate must reject read-only assignment in a merged directory, got {cli_diags:?}"
    );

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "sibling.crn");
    assert!(
        lsp_messages_contain(&lsp_diags, expected),
        "LSP must report the same read-only assignment, got {:?}",
        lsp_diags
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn nested_read_only_assignment_in_multi_file_directory_fails_validate() {
    let config = AttributeType::struct_(
        "Config".to_string(),
        vec![StructField::provider_populated("arn", AttributeType::string()).build()],
    );
    let schemas = single_schema_map(
        ResourceSchema::new("r.bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("config", config).build()),
    );
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.bucket {
    name = "configured"
}
"#,
        ),
        (
            "sibling.crn",
            r#"
test.r.bucket {
    name = "sibling"
    config = {
        arn = "provider-arn"
    }
}
"#,
        ),
    ]);

    let expected = "Attribute 'arn' is populated by the provider and cannot be set";
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&cli_diags, expected),
        "carina validate must reject nested read-only assignment in a merged directory, got {cli_diags:?}"
    );

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "sibling.crn");
    let diagnostic = lsp_diags
        .iter()
        .find(|diagnostic| diagnostic.message.contains(expected))
        .unwrap_or_else(|| {
            panic!(
                "LSP must report the same nested read-only assignment, got {:?}",
                lsp_diags
                    .iter()
                    .map(|diagnostic| &diagnostic.message)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "LSP parity includes hard-error severity for nested writability"
    );
}

// ============================================================================
// Scenario 6: Unknown attribute with `suggestion`
// ============================================================================

#[test]
fn unknown_attribute_emits_suggestion_parity() {
    let schemas = single_schema_map(
        ResourceSchema::new("r.suggester")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build()),
    );
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.suggester {
    name = "a"
    descritpion = "x"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let has_suggestion = |c: &dyn Fn(&str) -> bool| c("descritpion") && c("description");
    assert!(
        has_suggestion(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must suggest 'description', got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        has_suggestion(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must suggest 'description', got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario 3b: List<Struct> with nested-Struct field error (#2249)
// ============================================================================

fn list_struct_schemas() -> SchemaRegistry {
    let inner = AttributeType::struct_(
        "Inner".to_string(),
        vec![StructField::optional("leaf", AttributeType::int()).build()],
    );
    let outer = AttributeType::list(AttributeType::struct_(
        "Outer".to_string(),
        vec![
            StructField::optional("inner", inner).build(),
            StructField::optional("label", AttributeType::string()).build(),
        ],
    ));
    single_schema_map(
        ResourceSchema::new("r.list_nested")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("outer", outer)
                    .with_block_name("outer")
                    .build(),
            ),
    )
}

#[test]
fn list_struct_int_field_with_string_value_diagnoses_parity() {
    let schemas = list_struct_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.list_nested {
    name = "a"
    outer {
        label = "first"
        inner = { leaf = 30 }
    }
    outer {
        label = "second"
        inner = { leaf = "not-an-int" }
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("leaf") || c("Int") || c("not-an-int");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must diagnose inner.leaf int mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must diagnose inner.leaf int mismatch, got {:?}",
        cli_diags,
    );
}

fn list_struct_renamed_block_schemas() -> SchemaRegistry {
    let rule = AttributeType::list(AttributeType::struct_(
        "Rule".to_string(),
        vec![StructField::optional("days", AttributeType::int()).build()],
    ));
    single_schema_map(
        ResourceSchema::new("r.renamed_block")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("rules", rule)
                    .with_block_name("rule")
                    .build(),
            ),
    )
}

#[test]
fn list_struct_renamed_block_int_field_with_string_value_diagnoses_parity() {
    let schemas = list_struct_renamed_block_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.renamed_block {
    name = "a"
    rule {
        days = 7
    }
    rule {
        days = "not-an-int"
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("days") || c("Int") || c("not-an-int");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must diagnose rule.days int mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must diagnose rule.days int mismatch, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: WASM-plugin Custom validator bridge (#2354)
//
// A schema produced via the WASM plugin path carries `validate: None`
// on every `AttributeType::Custom`, with the real validator behind the
// factory's `validate_custom_type`. The validation pipeline must reach
// through the loaded `ProviderContext` resolver so a bad `vpc_id`
// value is rejected.
// ============================================================================

struct WasmStyleProviderFactory {
    name: String,
    schemas: Vec<ValidatedResourceSchema>,
}

impl ProviderFactory for WasmStyleProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        "WASM-style test provider"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        identity: &carina_core::schema::TypeIdentity,
        value: &carina_core::provider::CanonicalCustomTypeValue,
    ) -> carina_core::provider::ProviderResult<()> {
        // Mirror awscc's `prefixed` validator family: VpcId must start
        // with "vpc-".
        if identity.kind == "VpcId" && !value.as_str().starts_with("vpc-") {
            return Err(carina_core::provider::ProviderError::invalid_input(
                format!("Invalid vpc_id '{}': must start with 'vpc-'", value),
            ));
        }
        if identity.kind == "MaxAggregationInterval" {
            let value = value.as_str().parse::<i64>().map_err(|error| {
                carina_core::provider::ProviderError::invalid_input(format!(
                    "invalid aggregation interval: {error}"
                ))
            })?;
            if !matches!(value, 60 | 600) {
                return Err(carina_core::provider::ProviderError::invalid_input(
                    format!("max_aggregation_interval must be 60 or 600, got {value}"),
                ));
            }
        }
        Ok(())
    }
    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        self.schemas.clone()
    }
}

fn wasm_style_vpc_schema() -> ResourceSchema {
    let vpc_id_type = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        None,
        None,
    );
    ResourceSchema::new("ec2.security_group")
        .attribute(AttributeSchema::optional("group_description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_type).build())
}

fn wasm_style_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awsccmock",
        (wasm_style_vpc_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awsccmock",
        wasm_style_flow_log_schema()
            .validate()
            .expect("numeric bridge schema fixture must be valid"),
    );
    schemas
}

fn wasm_style_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".to_string(),
        schemas: vec![
            wasm_style_vpc_schema()
                .validate()
                .expect("factory test schema must be valid"),
            wasm_style_flow_log_schema()
                .validate()
                .expect("numeric bridge factory schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

fn wasm_style_engine() -> DiagnosticEngine {
    let schemas = wasm_style_schemas();
    let factories: Vec<Box<dyn ProviderFactory>> = wasm_style_factories();
    let provider_names = vec!["awsccmock".to_string()];
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(factories))
}

fn wasm_style_flow_log_schema() -> ResourceSchema {
    let identity = carina_core::schema::TypeIdentity::new(
        Some("aws"),
        ["ec2", "FlowLog"],
        "MaxAggregationInterval",
    );
    ResourceSchema::new("ec2.FlowLog").attribute(
        AttributeSchema::optional(
            "max_aggregation_interval",
            AttributeType::refined_int(Some(identity), None),
        )
        .build(),
    )
}

#[test]
fn wasm_plugin_custom_validator_rejects_invalid_vpc_id_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.security_group {
    group_description = "web sg"
    vpc_id            = "test"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "vpc_id") || lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must diagnose invalid vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "vpc_id") || cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must diagnose invalid vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn wasm_plugin_custom_validator_accepts_valid_vpc_id_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.security_group {
    group_description = "web sg"
    vpc_id            = "vpc-12345678"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        !lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must not diagnose a valid vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must not diagnose a valid vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn numeric_wasm_validator_accepts_60_in_cli_and_lsp() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.FlowLog {
    max_aggregation_interval = 60
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        !lsp_messages_contain(&lsp_diags, "max_aggregation_interval"),
        "LSP must accept numeric bridge value 60, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain(&cli_diags, "max_aggregation_interval"),
        "CLI validate must accept numeric bridge value 60, got {cli_diags:?}",
    );
}

#[test]
fn numeric_wasm_validator_rejects_61_in_cli_and_lsp() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.FlowLog {
    max_aggregation_interval = 61
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "max_aggregation_interval"),
        "LSP must reject numeric bridge value 61, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "max_aggregation_interval"),
        "CLI validate must reject numeric bridge value 61, got {cli_diags:?}",
    );
}

fn wasm_style_subnet_schema() -> ResourceSchema {
    let vpc_id_type = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        None,
        None,
    );
    ResourceSchema::new("ec2.subnet")
        .attribute(AttributeSchema::optional("vpc_ids", AttributeType::list(vpc_id_type)).build())
}

fn wasm_style_subnet_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awsccmock",
        (wasm_style_subnet_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas
}

fn wasm_style_subnet_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".to_string(),
        schemas: vec![
            wasm_style_subnet_schema()
                .validate()
                .expect("factory test schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

#[test]
fn wasm_plugin_custom_validator_rejects_invalid_vpc_id_inside_list_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.subnet {
    vpc_ids = ["vpc-good", "bogus"]
}
"#,
    )]);
    let schemas = wasm_style_subnet_schemas();
    let provider_names = vec!["awsccmock".to_string()];
    let engine = DiagnosticEngine::new(
        Arc::new(schemas),
        provider_names,
        Arc::new(wasm_style_subnet_factories()),
    );

    let lsp_diags = lsp_diagnostics(&engine, &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_subnet_factories(), &fixture);

    assert!(
        cli_messages_contain(&cli_diags, "bogus") || cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must diagnose invalid vpc_id inside list, got {:?}",
        cli_diags,
    );
    assert!(
        lsp_messages_contain(&lsp_diags, "bogus") || lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must diagnose invalid vpc_id inside list, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn wasm_plugin_custom_validator_reports_every_bad_item_in_list() {
    // Multi-item failure case: walk_custom_lookup should accumulate one
    // diagnostic per bad item instead of stopping at the first.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.subnet {
    vpc_ids = ["bad-1", "bad-2"]
}
"#,
    )]);
    let cli_diags = cli_diagnostics(wasm_style_subnet_factories(), &fixture);
    let bad_count = cli_diags
        .iter()
        .filter(|m| m.contains("bad-1") || m.contains("bad-2"))
        .count();
    assert_eq!(
        bad_count, 2,
        "CLI must report a diagnostic per bad list item, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: directional generic/specific assignments (#2358)
//
// `vpc_id: String = main.vpc_id` declares a `String`-typed export of a
// value whose actual type is `Custom { identity: VpcId }`. The
// declaration is a safe widening and therefore passes. A downstream
// consumer sees only `String`, so the reverse flow into a specific
// `Custom { VpcId }` sink remains rejected. Validation, LSP diagnostics,
// and completion must agree on that directionality.
// ============================================================================

// `vpc-` prefix validator — mirrors how the awscc plugin attaches its
// real `prefixed` validators. Used by #2358 fixtures so the literal-
// assignment test can prove the schema-attached `validate` closure
// actually runs (a noop validator would let the test pass for the
// wrong reason).
fn vpc_id_validate_2358(v: &Value) -> Result<(), String> {
    match v {
        Value::Concrete(ConcreteValue::String(s)) if s.expose_plaintext().starts_with("vpc-") => {
            Ok(())
        }
        Value::Concrete(ConcreteValue::String(s)) => Err(format!(
            "Invalid vpc_id '{}': must start with 'vpc-'",
            s.expose_plaintext()
        )),
        _ => Ok(()),
    }
}

fn vpc_id_custom_type_2358() -> AttributeType {
    AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(vpc_id_validate_2358),
        None,
    )
}

fn vpc_resource_schema_2358() -> ResourceSchema {
    ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom_type_2358()).build())
}

fn security_group_schema_2358() -> ResourceSchema {
    ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::optional("group_description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom_type_2358()).build())
}

fn schemas_2358() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc2358",
        (vpc_resource_schema_2358())
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc2358",
        (security_group_schema_2358())
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas
}

fn factories_2358() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awscc2358".to_string(),
        schemas: vec![
            vpc_resource_schema_2358()
                .validate()
                .expect("factory test schema must be valid"),
            security_group_schema_2358()
                .validate()
                .expect("factory test schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

fn engine_2358() -> DiagnosticEngine {
    DiagnosticEngine::new(
        Arc::new(schemas_2358()),
        vec!["awscc2358".to_string()],
        Arc::new(factories_2358()),
    )
}

#[test]
fn export_string_annotation_of_custom_value_widens_parity() {
    // The export declares `: String` but the rhs `main.vpc_id` carries
    // type `Custom { VpcId }`. This is source-specific → sink-generic,
    // so both validate and LSP diagnostics must accept the widening.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: String = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must accept Custom-to-String widening, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must accept Custom-to-String widening, got {:?}",
        cli_diags,
    );
}

#[test]
fn secret_valued_exports_are_rejected_with_cli_lsp_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let s = secret("topsecretXYZ")

exports {
    direct: String = s
    builtin: String = upper(s)
    interpolated: String = "x-${s}"
    inline_interpolated = "x-${secret("topsecretXYZ")}"
    nested_list: list(String) = [s]
    nested_map: map(String) = { key = s }
}
"#,
    )]);
    let engine = engine_with_schemas(SchemaRegistry::new());

    let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let cli = cli_diagnostics(vec![], &fixture);

    for name in [
        "direct",
        "builtin",
        "interpolated",
        "inline_interpolated",
        "nested_list",
        "nested_map",
    ] {
        let expected = format!("export '{name}' cannot hold a secret value");
        assert!(
            lsp.iter().any(|diagnostic| diagnostic.message == expected),
            "LSP missing {expected:?}: {lsp:#?}",
        );
        assert!(
            cli.iter().any(|diagnostic| diagnostic.contains(&expected)),
            "CLI missing {expected:?}: {cli:#?}",
        );
    }
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("topsecretXYZ")),
        "LSP leaked secret: {lsp:#?}",
    );
    assert!(
        cli.iter()
            .all(|diagnostic| !diagnostic.contains("topsecretXYZ")),
        "CLI leaked secret: {cli:#?}",
    );
}

#[test]
fn export_vpcid_annotation_of_custom_value_accepted_parity() {
    // Safe direction: the export declares `: VpcId`, matching the
    // rhs's actual type. Both validate and LSP must stay quiet.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: VpcId = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must not flag a same-type export as a mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must not flag a same-type export as a mismatch, got {:?}",
        cli_diags,
    );
}

#[test]
fn literal_valid_string_assignment_to_custom_receiver_still_validates() {
    // Existing `awscc.ec2.SecurityGroup { vpc_id = 'vpc-12345678' }`
    // pattern remains valid: a literal string in source becomes
    // `Value::Concrete(ConcreteValue::String(...))`, which validation handles via the schema-
    // attached `validate` closure on the Custom type — not via the
    // lifted module-boundary relation. The strictness fix must
    // not regress this path.
    //
    // The fixture's validator requires a `vpc-` prefix; a well-formed
    // value must pass. Paired with the malformed-literal test below
    // this proves the `validate` closure is actually being run (a
    // silent noop validator would let either test pass for the wrong
    // reason).
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awscc2358.ec2.SecurityGroup {
    group_description = "web sg"
    vpc_id            = "vpc-12345678"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "vpc_id"),
        "LSP must not flag a well-formed literal vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "vpc_id"),
        "CLI must not flag a well-formed literal vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn literal_malformed_string_assignment_still_runs_attached_validator() {
    // Companion to the well-formed test above. A malformed literal
    // (`'bad'` — no `vpc-` prefix) must trip the schema-attached
    // `validate` closure and surface in both surfaces. Together the
    // pair proves the literal-assignment path is genuinely intact:
    // the well-formed test is meaningful only because this one shows
    // the validator can fail.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awscc2358.ec2.SecurityGroup {
    group_description = "web sg"
    vpc_id            = "bad"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        lsp_messages_contain_ci(&lsp_diags, "vpc-"),
        "LSP must surface the vpc_id validator's prefix complaint, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain_ci(&cli_diags, "vpc-"),
        "CLI must surface the vpc_id validator's prefix complaint, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: rhs-driven inference for unannotated exports (#2361, stage 1
// of #2360)
//
// Closes the unannotated half of the soundness hole #2358 / PR #2359
// only partially closed: previously `exports { vpc_id = main.vpc_id }`
// (no annotation) was skipped by every type-checking predicate, so a
// downstream `vpc_id = network.vpc_id` against a `Custom{VpcId}`
// receiver passed through unchecked. After this stage, the inferer
// fills in the missing `type_expr` from the rhs whenever it can; if
// it cannot (e.g. dynamic builtin, heterogeneous list), validation
// surfaces a "type annotation required" error.
// ============================================================================

#[test]
fn unannotated_export_of_resource_ref_inferred_and_typechecked() {
    // `vpc_id = main.vpc_id` infers `VpcId` from the `Vpc` schema,
    // which makes the export carry that type even though the user
    // wrote no annotation. Both surfaces stay quiet.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must not flag a same-type unannotated export, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must not flag a same-type unannotated export, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "CLI must infer the type and not demand annotation, got {:?}",
        cli_diags,
    );
}

#[test]
fn unannotated_export_of_dynamic_lookup_demands_annotation() {
    // `lookup` is a `BuiltinReturnType::Any` builtin — the return type
    // depends on arguments. Unannotated, the inferer cannot determine
    // a static type and must surface "type annotation required".
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    zone_id = lookup({a = "1"}, "a", "default")
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        cli_messages_contain_ci(&cli_diags, "annotation required"),
        "CLI must demand a type annotation for an Any-returning builtin, got {:?}",
        cli_diags,
    );
    assert!(
        cli_messages_contain_ci(&cli_diags, "zone_id"),
        "diagnostic must name the offending export, got {:?}",
        cli_diags,
    );
    // validate / LSP parity: the same diagnostic must surface in-editor.
    assert!(
        lsp_messages_contain_ci(&lsp_diags, "annotation required"),
        "LSP must demand a type annotation for an Any-returning builtin, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        lsp_messages_contain_ci(&lsp_diags, "zone_id"),
        "LSP diagnostic must name the offending export, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn unannotated_export_of_literal_inferred_as_primitive() {
    // A literal-only rhs is inferable to its primitive type. The
    // export typechecks without annotation and without spurious
    // diagnostics.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    name = "carina"
    count = 3
    enabled = true
}
"#,
    )]);

    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "literals must infer cleanly, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "literals must not flag a mismatch, got {:?}",
        cli_diags,
    );
}

#[test]
fn explicit_annotation_is_not_overwritten_by_inference() {
    // With an explicit `: VpcId` the export keeps that declared type
    // even though the rhs is also `Custom{VpcId}` — confirm inference
    // doesn't race against user intent.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: VpcId = main.vpc_id
}
"#,
    )]);

    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "annotated same-type export must stay clean, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "annotation is present — must not demand one, got {:?}",
        cli_diags,
    );
}

#[test]
fn declared_export_unknown_binding_reports_exactly_one_diagnostic_per_frontend() {
    let fixture = write_fixture(&[(
        "main.crn",
        "exports {\n  x: String = typo_binding.attr\n}\n",
    )]);
    let schemas = SchemaRegistry::new();

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);

    assert_eq!(
        lsp.len(),
        1,
        "LSP must report the identifier-scope error once: {lsp:#?}",
    );
    assert_eq!(
        cli.len(),
        1,
        "CLI must report the identifier-scope error once: {cli:#?}",
    );
    assert!(
        lsp[0].message.contains("Undefined") && lsp[0].message.contains("typo_binding"),
        "unexpected LSP diagnostic: {lsp:#?}",
    );
    assert!(
        cli[0].contains("Undefined") && cli[0].contains("typo_binding"),
        "unexpected CLI diagnostic: {cli:#?}",
    );
}
