//! Multi-file `.crn` fixture helpers shared by integration tests that
//! exercise `DiagnosticEngine::analyze_with_filename` against the
//! directory-shaped layout the project's invariant requires
//! (`main.crn` + sibling files like `providers.crn`, `exports.crn`).

use std::collections::HashMap;
use std::sync::Arc;

use carina_core::schema::ResourceSchema;
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;
use tempfile::TempDir;
use tower_lsp::lsp_types::Diagnostic;

/// Lay a multi-file fixture under `tempfile::tempdir()` and return the
/// owning `TempDir`. Caller keeps it alive for the duration of the test.
#[allow(dead_code)]
pub fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("write fixture file");
    }
    dir
}

/// Read one file inside the fixture and feed it through `analyze` with
/// `base_path` set so the engine sees sibling files.
#[allow(dead_code)]
pub fn analyze(engine: &DiagnosticEngine, fixture: &TempDir, file_name: &str) -> Vec<Diagnostic> {
    let path = fixture.path().join(file_name);
    let text = std::fs::read_to_string(&path).expect("read fixture file");
    let doc = Document::new(
        text,
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    engine.analyze_with_filename(&doc, Some(file_name), Some(fixture.path()))
}

/// Build a `DiagnosticEngine` from in-memory schemas. Provider names
/// are derived from the `<provider>.<...>` keys so the engine can
/// recognise the synthetic providers used in tests.
#[allow(dead_code)]
pub fn engine_with_schemas(schemas: HashMap<String, ResourceSchema>) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .keys()
        .filter_map(|k| k.split('.').next())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(String::from)
        .collect();
    DiagnosticEngine::new(Arc::new(schemas), provider_names, Arc::new(vec![]))
}

/// Wrap one `ResourceSchema` in a `HashMap` keyed by its
/// `resource_type`. Convenience for single-resource scenarios.
#[allow(dead_code)]
pub fn single_schema_map(schema: ResourceSchema) -> HashMap<String, ResourceSchema> {
    let mut schemas = HashMap::new();
    schemas.insert(schema.resource_type.clone(), schema);
    schemas
}
