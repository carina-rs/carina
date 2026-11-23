//! The production LSP path must report the same authored-call finding as validate.

#[path = "../../carina-cli/tests/support/fn_type_promise.rs"]
#[allow(dead_code)]
mod fixture;

use std::sync::Arc;

use carina_core::{
    EnvironmentSnapshot, parser::ProviderContext, provider::*, schema::SchemaRegistry,
};
use carina_lsp::{document::Document, test_support::Diagnostics};
use fixture::{Directory, PromiseFactory, Scope};
use tower_lsp::lsp_types::DiagnosticSeverity;

#[test]
fn tp1_lsp_rule3_parameter_diagnostic_at_authored_call() {
    let directory = Directory::new(Scope::Root);
    directory.write_source(&format!(
        "{}fn promise(value: {}): String {{ 'accepted' }}\ntest.Sink {{ name = 'sink' value = promise(src.identifier) }}\n",
        fixture::SOURCE,
        fixture::ENVIRONMENT,
    ));
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(PromiseFactory) as Box<dyn ProviderFactory>]),
        SchemaRegistry::new(),
        EnvironmentSnapshot::empty(),
    );
    let engine = Diagnostics::new(directory.root(), Arc::new(loaded), vec!["test".into()]);
    let path = directory.source_file();
    let document = Document::new_with_path(
        std::fs::read_to_string(&path).unwrap(),
        path.clone(),
        Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
    );
    let errors: Vec<_> = engine
        .analyze(&document, Some(&path))
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(
        errors[0].message,
        "function 'promise': parameter 'value' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value."
    );
    assert_eq!(errors[0].range.start.line, 2, "{errors:#?}");
}
