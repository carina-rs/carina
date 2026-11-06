//! carina#4305: provider-config causes must not expose secrets in diagnostics.

use std::sync::Arc;

use carina_core::provider::{ProviderFactory, loaded_provider_snapshot_from_factories};
use carina_core::schema::SchemaRegistry;
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;

#[path = "../../carina-provider-mock/tests/support/provider_text_secret.rs"]
mod fixture;
use fixture::{EchoFactory, Failure, MASK, PUBLIC, SECRET};

#[test]
fn provider_config_cause_secret_is_redacted_from_lsp_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("providers.crn");
    let source = format!("provider mock {{\n  password = secret(\"{SECRET}\")\n}}\n");
    std::fs::write(&path, &source).unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        "mock.test.resource { name = \"probe\" }\n",
    )
    .unwrap();
    let factories = vec![Box::new(EchoFactory(Failure::ConfigCause)) as Box<dyn ProviderFactory>];
    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
    let document = Document::new_with_path(
        source,
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let diagnostics = engine.analyze(&document, Some(&path));
    let diagnostic = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("failed validation"))
        .unwrap_or_else(|| {
            panic!("provider config validation diagnostic missing: {diagnostics:#?}")
        });
    assert!(
        diagnostic.message.contains(PUBLIC),
        "non-secret cause context was lost: {diagnostic:#?}"
    );
    assert!(
        !diagnostic.message.contains(SECRET),
        "provider config secret plaintext leaked to LSP diagnostic: {diagnostic:#?}"
    );
    assert!(
        diagnostic.message.contains(MASK),
        "missing {MASK} in diagnostic: {diagnostic:#?}"
    );
}
