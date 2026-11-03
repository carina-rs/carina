#[path = "../../carina-core/tests/support/identity_edges.rs"]
pub mod identity_edges;

use carina_lsp::{
    completion::CompletionProvider, diagnostics::DiagnosticEngine, document::Document,
};
use std::sync::Arc;
use tower_lsp::lsp_types::Position;

#[test]
fn identity_edge_diagnostics_match_validation() {
    let loaded = Arc::new(identity_edges::snapshot(&[("mock", true)]));
    let engine = DiagnosticEngine::new(Arc::clone(&loaded), vec!["mock".into()]);
    for (attribute, source, accepted) in [
        ("bucket", "new_name", true),
        ("new_bucket", "old_name", false),
        ("bucket", "unrelated", false),
    ] {
        let text = format!(
            "provider mock {{}}\nlet producer = mock.Bucket {{}}\nlet consumer = mock.Bucket {{ {attribute} = producer.{source} }}\n"
        );
        let doc = Document::new_with_path(
            text,
            std::path::PathBuf::from("test.crn"),
            Arc::new(loaded.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&doc, None);
        if accepted {
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        } else {
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("Type mismatch")),
                "{diagnostics:#?}"
            );
        }
    }
}

#[test]
fn identity_edge_completion_offers_forward_output_and_excludes_reverse() {
    let loaded = identity_edges::snapshot(&[("mock", true)]);
    let provider = CompletionProvider::new(
        loaded.schemas().schemas_arc(),
        vec!["mock".into()],
        vec![],
        vec![],
    );
    for (attribute, offered, excluded) in [
        ("bucket", "producer.new_name", "producer.unrelated"),
        ("new_bucket", "producer.new_name", "producer.old_name"),
    ] {
        let text = format!(
            "provider mock {{}}\nlet producer = mock.Bucket {{}}\nlet consumer = mock.Bucket {{\n  {attribute} = \n}}\n"
        );
        let position = Position::new(3, (attribute.len() + 5) as u32);
        let doc = Document::new_with_path(
            text,
            std::path::PathBuf::from("test.crn"),
            Arc::new(loaded.provider_context().as_provider_context().clone()),
        );
        let items = provider.complete(&doc, position, None, doc.provider_context());
        assert!(
            items.iter().any(|item| item.label == offered),
            "{offered} absent: {items:#?}"
        );
        assert!(
            !items.iter().any(|item| item.label == excluded),
            "{excluded} offered: {items:#?}"
        );
    }
}

#[test]
fn identity_edge_diagnostics_and_completion_keep_the_sink_factory() {
    let loaded = Arc::new(identity_edges::snapshot(&[
        ("mock", true),
        ("other", false),
    ]));
    let engine = DiagnosticEngine::new(Arc::clone(&loaded), vec!["mock".into(), "other".into()]);
    let text = "provider mock {}\nprovider other {}\nlet producer = mock.Bucket {}\nlet consumer = other.Bucket { bucket = producer.new_name }\n";
    let doc = Document::new_with_path(
        text.into(),
        std::path::PathBuf::from("test.crn"),
        Arc::new(loaded.provider_context().as_provider_context().clone()),
    );
    let diagnostics = engine.analyze(&doc, None);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("got mock.Bucket.NewName")),
        "{diagnostics:#?}"
    );
    let provider = CompletionProvider::new(
        loaded.schemas().schemas_arc(),
        vec!["mock".into(), "other".into()],
        vec![],
        vec![],
    );
    let text = "provider mock {}\nprovider other {}\nlet producer = mock.Bucket {}\nlet consumer = other.Bucket {\n  bucket = \n}\n";
    let doc = Document::new_with_path(
        text.into(),
        std::path::PathBuf::from("test.crn"),
        Arc::new(loaded.provider_context().as_provider_context().clone()),
    );
    let items = provider.complete(&doc, Position::new(4, 11), None, doc.provider_context());
    assert!(
        !items.iter().any(|item| item.label == "producer.new_name"),
        "{items:#?}"
    );
    assert!(
        items.iter().any(|item| item.label == "producer.old_name"),
        "{items:#?}"
    );
}
