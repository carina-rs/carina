//! Multi-file `.crn` fixture helpers shared by integration tests that
//! exercise `DiagnosticEngine::analyze` against the
//! directory-shaped layout the project's invariant requires
//! (`main.crn` + sibling files like `providers.crn`, `exports.crn`).

use std::sync::Arc;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resource::Value;
use carina_core::schema::{
    ResourceSchema, SchemaRegistry, ValidatedConfigSchema, ValidatedResourceSchema,
};
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;
use indexmap::IndexMap;
use tempfile::TempDir;
use tower_lsp::lsp_types::Diagnostic;

struct SchemaFixtureFactory {
    name: String,
    schemas: Vec<ValidatedResourceSchema>,
}

impl ProviderFactory for SchemaFixtureFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        unimplemented!("schema fixture factories are never instantiated")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        self.schemas.clone()
    }
}

fn factories_for(schemas: &SchemaRegistry) -> Vec<Box<dyn ProviderFactory>> {
    let mut by_provider = std::collections::HashMap::<String, Vec<ValidatedResourceSchema>>::new();
    for (provider, _resource_type, _kind, schema) in schemas.iter() {
        by_provider
            .entry(provider.to_string())
            .or_default()
            .push(schema.clone());
    }
    by_provider
        .into_iter()
        .map(|(name, schemas)| {
            Box::new(SchemaFixtureFactory { name, schemas }) as Box<dyn ProviderFactory>
        })
        .collect()
}

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
    let doc = Document::new_with_path(
        text,
        path,
        Arc::new(carina_core::parser::ProviderContext::default()),
    );
    engine.analyze(&doc, Some(&fixture.path().join(file_name)))
}

/// Build a `DiagnosticEngine` from in-memory schemas. Provider names
/// are derived from registered schema entries so the engine can
/// recognise the synthetic providers used in tests.
#[allow(dead_code)]
pub fn engine_with_schemas(schemas: SchemaRegistry) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .iter()
        .map(|(provider, _, _, _)| provider.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let factories = factories_for(&schemas);
    if provider_names.is_empty() {
        DiagnosticEngine::without_loaded_provider_schemas()
    } else {
        let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(factories),
            std::iter::empty(),
        );
        DiagnosticEngine::new(Arc::new(loaded_providers), provider_names)
    }
}

/// Wrap one `ResourceSchema` in a `SchemaRegistry` registered under the
/// `test` provider — the standard provider name for synthetic test fixtures
/// in this crate.
#[allow(dead_code)]
pub fn single_schema_map(schema: ResourceSchema) -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    registry
}
