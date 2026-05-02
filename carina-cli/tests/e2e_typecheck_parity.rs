//! End-to-end CLI parity tests for the type-check pipeline (#2247).
//!
//! Each scenario builds the same directory-shaped fixture as
//! `carina-lsp/tests/e2e_typecheck.rs` (#2215), runs both:
//!
//! 1. `DiagnosticEngine::analyze_with_filename` (LSP path)
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

use std::collections::HashMap;
use std::sync::Arc;

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer,
};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
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

fn engine_with_schemas(schemas: HashMap<String, ResourceSchema>) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .keys()
        .filter_map(|k| k.split('.').next())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(String::from)
        .collect();
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(vec![]))
}

fn single_schema_map(schema: ResourceSchema) -> HashMap<String, ResourceSchema> {
    let mut schemas = HashMap::new();
    schemas.insert(schema.resource_type.clone(), schema);
    schemas
}

/// Build a `ProviderFactory` set covering every provider name implied by
/// the schemas. The CLI pipeline keys schemas by provider, so each
/// distinct prefix (`test`, `aws`, ...) needs its own factory entry.
fn factories_for(schemas: &HashMap<String, ResourceSchema>) -> Vec<Box<dyn ProviderFactory>> {
    let mut by_provider: HashMap<String, Vec<ResourceSchema>> = HashMap::new();
    for (key, schema) in schemas {
        let provider = key.split('.').next().unwrap_or("test").to_string();
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
    let doc = Document::new(
        text,
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    engine.analyze_with_filename(&doc, Some(file_name), Some(fixture.path()))
}

/// Run the CLI path on a fixture directory and return the diagnostic
/// strings the CLI would surface.
fn cli_diagnostics(factories: Vec<Box<dyn ProviderFactory>>, fixture: &TempDir) -> Vec<String> {
    let path = fixture.path().to_path_buf();
    carina_cli::commands::validate::validate_with_factories(&path, factories)
}

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

// ============================================================================
// Test ProviderFactory — schema injection without WASM
// ============================================================================

struct TestProviderFactory {
    name: String,
    schemas: Vec<ResourceSchema>,
}

impl ProviderFactory for TestProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "Test provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }

    fn create_provider(
        &self,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        Box::pin(async { Box::new(NoopProvider) as Box<dyn Provider> })
    }

    fn create_normalizer(
        &self,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
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
        _id: &ResourceId,
        _identifier: Option<&str>,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn read_data_source(
        &self,
        _r: &Resource,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn create(&self, _r: &Resource) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _from: &State,
        _to: &Resource,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _lifecycle: &carina_core::resource::LifecycleConfig,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<()>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
}

// ============================================================================
// Scenario 1: StringEnum bare / TypeQualified / fully-qualified all pass
// ============================================================================

fn enum_schemas() -> HashMap<String, ResourceSchema> {
    let mode = AttributeType::StringEnum {
        name: "Mode".to_string(),
        values: vec!["fast".to_string(), "slow".to_string()],
        namespace: Some("test.r".to_string()),
        to_dsl: None,
    };
    single_schema_map(
        ResourceSchema::new("test.r.mode_holder")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("mode", mode)),
    )
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

// ============================================================================
// Scenario 2: Custom type with `to_dsl` normalization (Region-like)
// ============================================================================

fn region_schemas() -> HashMap<String, ResourceSchema> {
    fn validate_region(v: &Value) -> Result<(), String> {
        const VALID: &[&str] = &["ap-northeast-1", "us-west-2"];
        if let Value::String(s) = v {
            let normalized = carina_core::utils::extract_enum_value(s).replace('_', "-");
            if VALID.contains(&normalized.as_str()) {
                return Ok(());
            }
            return Err(format!("invalid Region '{}'", s));
        }
        Err("expected string".to_string())
    }
    fn to_dsl(s: &str) -> String {
        s.replace('-', "_")
    }

    let region_custom = AttributeType::Custom {
        semantic_name: Some("Region".to_string()),
        base: Box::new(AttributeType::String),
        pattern: None,
        length: None,
        validate: validate_region,
        namespace: Some("test".to_string()),
        to_dsl: Some(to_dsl),
    };

    single_schema_map(
        ResourceSchema::new("test.r.region_holder")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("region", region_custom)),
    )
}

#[test]
fn region_accepts_bare_and_typequalified_forms_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.region_holder {
    name = "a"
    region = ap_northeast_1
}
test.r.region_holder {
    name = "b"
    region = Region.us_west_2
}
test.r.region_holder {
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
test.r.region_holder {
    name = "a"
    region = "ap-northeast-1"
}
test.r.region_holder {
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

fn nested_struct_schemas() -> HashMap<String, ResourceSchema> {
    let inner = AttributeType::Struct {
        name: "Inner".to_string(),
        fields: vec![StructField::new("leaf", AttributeType::Int)],
    };
    let outer = AttributeType::Struct {
        name: "Outer".to_string(),
        fields: vec![
            StructField::new("inner", inner),
            StructField::new("label", AttributeType::String),
        ],
    };

    single_schema_map(
        ResourceSchema::new("test.r.nested")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("outer", outer)),
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

fn union_schemas() -> HashMap<String, ResourceSchema> {
    let union = AttributeType::Union(vec![AttributeType::Int, AttributeType::String]);
    single_schema_map(
        ResourceSchema::new("test.r.union")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("value", union)),
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

// ============================================================================
// Scenario 5: ResourceRef across sibling files
// ============================================================================

fn resource_ref_schemas() -> HashMap<String, ResourceSchema> {
    let producer = ResourceSchema::new("test.r.producer")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("id", AttributeType::String).read_only());
    let consumer = ResourceSchema::new("test.r.consumer")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("target_id", AttributeType::String));
    let mut schemas = HashMap::new();
    schemas.insert(producer.resource_type.clone(), producer);
    schemas.insert(consumer.resource_type.clone(), consumer);
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

// ============================================================================
// Scenario 6: Unknown attribute with `suggestion`
// ============================================================================

#[test]
fn unknown_attribute_emits_suggestion_parity() {
    let schemas = single_schema_map(
        ResourceSchema::new("test.r.suggester")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("description", AttributeType::String)),
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

fn list_struct_schemas() -> HashMap<String, ResourceSchema> {
    let inner = AttributeType::Struct {
        name: "Inner".to_string(),
        fields: vec![StructField::new("leaf", AttributeType::Int)],
    };
    let outer = AttributeType::list(AttributeType::Struct {
        name: "Outer".to_string(),
        fields: vec![
            StructField::new("inner", inner),
            StructField::new("label", AttributeType::String),
        ],
    });
    single_schema_map(
        ResourceSchema::new("test.r.list_nested")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("outer", outer).with_block_name("outer")),
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

fn list_struct_renamed_block_schemas() -> HashMap<String, ResourceSchema> {
    let rule = AttributeType::list(AttributeType::Struct {
        name: "Rule".to_string(),
        fields: vec![StructField::new("days", AttributeType::Int)],
    });
    single_schema_map(
        ResourceSchema::new("test.r.renamed_block")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("rules", rule).with_block_name("rule")),
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
