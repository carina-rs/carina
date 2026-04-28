//! End-to-end type-check tests covering parse → resolve → validate (#2215).
//!
//! Each scenario builds a directory-shaped fixture (`main.crn`,
//! `exports.crn`, `providers.crn`) under `tempfile::tempdir()` and runs the
//! LSP `DiagnosticEngine::analyze_with_filename` against it. The point is to
//! catch interaction bugs between parsing, resolution, and schema validation
//! that the existing per-`AttributeType::validate` unit tests miss — and to
//! enforce the "directory-scoped, never single-file" rule from `CLAUDE.md`
//! for type checking.
//!
//! CLI parity (`carina validate` produces the same diagnostic set) is left
//! to a follow-up because exercising the CLI binary in tests requires a
//! provider plugin that is not built in this repo. These LSP-side tests
//! still pin the parse → resolve → validate pipeline end-to-end.

use std::collections::HashMap;
use std::sync::Arc;

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;
use tempfile::TempDir;

/// Lay a multi-file fixture mirroring `infra/aws/management/<dir>/`:
/// `main.crn`, `exports.crn`, `providers.crn`. Returns the temp dir
/// (kept alive by the caller) plus the absolute path it sits at.
fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("write fixture file");
    }
    dir
}

/// Build a `Document` from one file inside the fixture and feed it through
/// the engine with `base_path` set so directory-scoped checks see the
/// sibling files.
fn analyze(
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

// Mirrors `custom_engine` in `carina-lsp/src/diagnostics/tests/mod.rs`. The
// in-crate test helpers there are `pub(super)` and not reachable from
// `tests/`, so an integration test has to redefine this. Keep the two
// shapes in sync if either one grows fields.
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

/// Wrap one `ResourceSchema` in a `HashMap` keyed by its `resource_type`,
/// so each scenario only has to spell `"test.r.<thing>"` once. The
/// double-spelling is a real footgun: an off-by-one between the key and
/// `ResourceSchema::new` argument silently makes the engine treat the
/// resource as unknown.
fn single_schema_map(schema: ResourceSchema) -> HashMap<String, ResourceSchema> {
    let mut schemas = HashMap::new();
    schemas.insert(schema.resource_type.clone(), schema);
    schemas
}

fn count_with(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> usize {
    diags
        .iter()
        .filter(|d| d.message.contains(substring))
        .count()
}

/// Cheap helper for assertion failure messages: pull just the messages out
/// of a diagnostic slice for `{:?}` printing, so each test doesn't have to
/// build the same `Vec<&String>` inline.
fn messages_of(diags: &[tower_lsp::lsp_types::Diagnostic]) -> Vec<&String> {
    diags.iter().map(|d| &d.message).collect()
}

// ---------------------------------------------------------------
// Scenario 1: StringEnum with bare / TypeQualified / fully-qualified
// ---------------------------------------------------------------

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
fn enum_bare_typequalified_fully_qualified_all_pass() {
    let engine = engine_with_schemas(enum_schemas());
    // All three accepted shapes side-by-side. None should produce a Mode diag.
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
    let diags = analyze(&engine, &fixture, "main.crn");
    assert_eq!(
        count_with(&diags, "Mode"),
        0,
        "expected no Mode diagnostics, got: {:?}",
        messages_of(&diags),
    );
}

#[test]
fn enum_invalid_value_reports_diagnostic() {
    let engine = engine_with_schemas(enum_schemas());
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.mode_holder {
    name = "a"
    mode = "bogus"
}
"#,
    )]);
    let diags = analyze(&engine, &fixture, "main.crn");
    // The bad value must surface in a diagnostic so users can locate it.
    assert!(
        diags.iter().any(|d| d.message.contains("bogus")),
        "expected diagnostic mentioning the bad value `bogus`, got: {:?}",
        messages_of(&diags),
    );
}

// ---------------------------------------------------------------
// Scenario 2: Custom type with `to_dsl` normalization (Region-like)
// ---------------------------------------------------------------

fn region_schemas() -> HashMap<String, ResourceSchema> {
    // Validator accepts the canonical DSL form `test.Region.ap_northeast_1`
    // that the schema's namespace + name produces from a bare identifier.
    // The `to_dsl` callback is what real AWS Region uses to normalise
    // hyphenated AWS strings (`ap-northeast-1`) into the underscore DSL
    // form before validation; we mirror that shape.
    fn validate_region(v: &carina_core::resource::Value) -> Result<(), String> {
        if let carina_core::resource::Value::String(s) = v {
            if s == "test.Region.ap_northeast_1" || s == "test.Region.us_west_2" {
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
fn region_accepts_bare_and_typequalified_forms() {
    let engine = engine_with_schemas(region_schemas());
    // Bare and `TypeName.member` shorthands both resolve to the canonical
    // `test.Region.ap_northeast_1` and pass the validator.
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
    let diags = analyze(&engine, &fixture, "main.crn");
    assert_eq!(
        count_with(&diags, "Region") + count_with(&diags, "region"),
        0,
        "expected no Region diagnostics, got: {:?}",
        messages_of(&diags),
    );
}

#[test]
fn region_rejects_unknown_value() {
    let engine = engine_with_schemas(region_schemas());
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.region_holder {
    name = "a"
    region = mars_1
}
"#,
    )]);
    let diags = analyze(&engine, &fixture, "main.crn");
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("Region") || d.message.contains("mars")),
        "expected a diagnostic mentioning the bad region, got: {:?}",
        messages_of(&diags),
    );
}

// ---------------------------------------------------------------
// Scenario 3: nested-Struct field type mismatch
//
// Issue body asks for `List<Struct>` here, but list-literal fixtures
// (`outer = [{...}]`) trigger the LSP prefer-block-syntax warning before
// the type-check reaches the nested field, so this scenario uses two
// nested single Structs instead. A `List<Struct>` variant via a
// `with_block_name`-flagged StructField is left to a follow-up.
// ---------------------------------------------------------------

fn nested_struct_schemas() -> HashMap<String, ResourceSchema> {
    // Single Struct holding another single Struct — keeps the fixture in
    // block syntax (no list literals) so the test exercises only the
    // nested-Struct type-check path, not the prefer-block-syntax warning.
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
fn nested_struct_int_field_with_string_value_diagnoses() {
    let engine = engine_with_schemas(nested_struct_schemas());
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.nested {
    name = "a"
    outer {
        label = "x"
        inner {
            leaf = "not-an-int"
        }
    }
}
"#,
    )]);
    let diags = analyze(&engine, &fixture, "main.crn");
    // The mismatch is on `leaf` which is an `Int` field; the message must
    // anchor on the field name OR the offending value.
    assert!(
        diags.iter().any(|d| d.message.contains("leaf")
            || d.message.contains("Int")
            || d.message.contains("not-an-int")),
        "expected diagnostic for nested Int field type mismatch, got: {:?}",
        messages_of(&diags),
    );
}

// ---------------------------------------------------------------
// Scenario 4: Union with multiple member candidates
// ---------------------------------------------------------------

fn union_schemas() -> HashMap<String, ResourceSchema> {
    let union = AttributeType::Union(vec![AttributeType::Int, AttributeType::String]);
    single_schema_map(
        ResourceSchema::new("test.r.union")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("value", union)),
    )
}

#[test]
fn union_accepts_either_member() {
    let engine = engine_with_schemas(union_schemas());
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
    let diags = analyze(&engine, &fixture, "main.crn");
    let value_diags = count_with(&diags, "Type mismatch");
    assert_eq!(
        value_diags,
        0,
        "expected no Type mismatch diagnostics on union members, got: {:?}",
        messages_of(&diags),
    );
}

#[test]
fn union_rejects_non_member_type() {
    let engine = engine_with_schemas(union_schemas());
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.union {
    name = "a"
    value = true
}
"#,
    )]);
    let diags = analyze(&engine, &fixture, "main.crn");
    // Bool doesn't match any Union(Int, String) member, so the engine
    // should anchor on the `value` field or the rejected `Bool` type.
    assert!(
        diags.iter().any(|d| d.message.contains("value")
            || d.message.contains("Bool")
            || d.message.contains("Union")),
        "expected diagnostic for Bool not matching Union<Int, String>, got: {:?}",
        messages_of(&diags),
    );
}

// ---------------------------------------------------------------
// Scenario 5: ResourceRef pointing to a binding declared in a sibling file
// ---------------------------------------------------------------

fn resource_ref_schemas() -> HashMap<String, ResourceSchema> {
    // Producer: declares a `name` and an `id` attribute marked read-only
    // (provider-computed). Consumers reference it via `<binding>.id`.
    let producer = ResourceSchema::new("test.r.producer")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("id", AttributeType::String).read_only());

    // Consumer: takes a string `target_id`. The fixture below feeds it the
    // producer's computed `id` via a ResourceRef declared in a sibling file.
    let consumer = ResourceSchema::new("test.r.consumer")
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("target_id", AttributeType::String));

    let mut schemas = HashMap::new();
    schemas.insert(producer.resource_type.clone(), producer);
    schemas.insert(consumer.resource_type.clone(), consumer);
    schemas
}

#[test]
fn resource_ref_across_sibling_files_resolves() {
    let engine = engine_with_schemas(resource_ref_schemas());
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
    let diags = analyze(&engine, &fixture, "main.crn");
    let undefined = diags
        .iter()
        .filter(|d| d.message.contains("upstream") && d.message.contains("Undefined"))
        .count();
    assert_eq!(
        undefined,
        0,
        "expected `upstream` to resolve from sibling file; got: {:?}",
        messages_of(&diags),
    );
}

// ---------------------------------------------------------------
// Scenario 6: Unknown attribute with `suggestion`
// ---------------------------------------------------------------

#[test]
fn unknown_attribute_emits_suggestion() {
    let engine = engine_with_schemas(single_schema_map(
        ResourceSchema::new("test.r.suggester")
            .attribute(AttributeSchema::new("name", AttributeType::String))
            .attribute(AttributeSchema::new("description", AttributeType::String)),
    ));
    // Misspell `description` as `descritpion` — engine should suggest the
    // correct name in its diagnostic message.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.suggester {
    name = "a"
    descritpion = "x"
}
"#,
    )]);
    let diags = analyze(&engine, &fixture, "main.crn");
    let suggestion_hit = diags
        .iter()
        .find(|d| d.message.contains("descritpion") && d.message.contains("description"));
    assert!(
        suggestion_hit.is_some(),
        "expected an unknown-attribute diagnostic suggesting `description`, got: {:?}",
        messages_of(&diags),
    );
}
