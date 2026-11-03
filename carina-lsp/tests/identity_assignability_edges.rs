#[path = "../../carina-core/tests/support/identity_edges.rs"]
pub mod identity_edges;

use carina_core::parser::ProviderContext;
use carina_lsp::{
    Backend, backend::FactoryBuilder, diagnostics::DiagnosticEngine, document::Document,
};
use std::{path::PathBuf, sync::Arc};
use tempfile::TempDir;
use tower_lsp::{LanguageServer, LspService, lsp_types::*};

struct Scenario {
    directory: TempDir,
    owners: Vec<(&'static str, bool)>,
}

impl Scenario {
    fn new(owners: &[(&'static str, bool)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let providers = owners
            .iter()
            .map(|(owner, _)| format!("provider {owner} {{}}\n"))
            .collect::<String>();
        std::fs::write(directory.path().join("providers.crn"), providers).unwrap();
        std::fs::write(
            directory.path().join("producer.crn"),
            "let producer = mock.Bucket {}\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("consumer.crn"), "").unwrap();
        Self {
            directory,
            owners: owners.to_vec(),
        }
    }

    fn consumer_path(&self) -> PathBuf {
        self.directory.path().join("consumer.crn")
    }

    fn analyze(&self, text: &str) -> Vec<Diagnostic> {
        let loaded = Arc::new(identity_edges::snapshot(&self.owners));
        let engine = DiagnosticEngine::new(
            loaded,
            self.owners
                .iter()
                .map(|(owner, _)| (*owner).into())
                .collect(),
        );
        let path = self.consumer_path();
        let doc = Document::new_with_path(
            text.into(),
            path.clone(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        engine.analyze(&doc, Some(&path))
    }

    async fn complete(&self, text: &str, position: Position) -> Vec<CompletionItem> {
        let owners = self.owners.clone();
        let builder: FactoryBuilder = Arc::new(move |_| {
            let factories = owners
                .iter()
                .map(|&(owner, edges)| {
                    Box::new(identity_edges::EdgeFactory { owner, edges })
                        as Box<dyn carina_core::provider::ProviderFactory>
                })
                .collect();
            (factories, Default::default(), Vec::new())
        });
        let (service, socket) = LspService::new(|client| {
            Backend::new(
                client,
                ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
                Some(builder),
            )
        });
        drop(socket);
        let backend = service.inner();
        backend
            .initialize(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: Url::from_directory_path(self.directory.path()).unwrap(),
                    name: "identity edges".into(),
                }]),
                ..Default::default()
            })
            .await
            .unwrap();
        backend
            .did_change_watched_files(DidChangeWatchedFilesParams {
                changes: vec![FileEvent {
                    uri: Url::from_file_path(self.directory.path().join("providers.crn")).unwrap(),
                    typ: FileChangeType::CHANGED,
                }],
            })
            .await;
        let uri = Url::from_file_path(self.consumer_path()).unwrap();
        backend
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem::new(uri.clone(), "carina".into(), 1, text.into()),
            })
            .await;
        match backend
            .completion(CompletionParams {
                text_document_position: TextDocumentPositionParams::new(
                    TextDocumentIdentifier::new(uri),
                    position,
                ),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            })
            .await
            .unwrap()
            .unwrap()
        {
            CompletionResponse::Array(items) => items,
            CompletionResponse::List(list) => list.items,
        }
    }
}

#[test]
fn identity_edge_diagnostics_match_validation() {
    let scenario = Scenario::new(&[("mock", true)]);
    for (attribute, source, accepted) in [
        ("bucket", "new_name", true),
        ("new_bucket", "old_name", false),
        ("bucket", "unrelated", false),
    ] {
        let text = format!("let consumer = mock.Bucket {{ {attribute} = producer.{source} }}\n");
        let diagnostics = scenario.analyze(&text);
        if accepted {
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        } else {
            assert_eq!(
                diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.message.contains("Type mismatch"))
                    .count(),
                1,
                "{diagnostics:#?}"
            );
        }
    }
}

#[tokio::test]
async fn identity_edge_completion_offers_forward_output_and_excludes_reverse() {
    let scenario = Scenario::new(&[("mock", true)]);
    for (attribute, offered, excluded) in [
        ("bucket", "producer.new_name", "producer.unrelated"),
        ("new_bucket", "producer.new_name", "producer.old_name"),
    ] {
        let text = format!("let consumer = mock.Bucket {{\n  {attribute} = \n}}\n");
        let items = scenario
            .complete(&text, Position::new(1, (attribute.len() + 5) as u32))
            .await;
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
fn identity_edge_diagnostics_retain_loaded_catalog_with_incomplete_sibling() {
    let scenario = Scenario::new(&[("mock", true)]);
    std::fs::write(
        scenario.directory.path().join("draft.crn"),
        "let draft = mock.Bucket {\n  bucket = \n}\n",
    )
    .unwrap();
    let diagnostics = scenario.analyze("fn check(value: BareUnknown) { value }\n");
    let unknown = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("unknown custom type 'BareUnknown'")
        })
        .collect::<Vec<_>>();
    assert_eq!(unknown.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        unknown[0].severity,
        Some(DiagnosticSeverity::ERROR),
        "{diagnostics:#?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("was deferred")),
        "{diagnostics:#?}"
    );
}

#[tokio::test]
async fn identity_edge_diagnostics_and_completion_keep_the_sink_factory() {
    let scenario = Scenario::new(&[("mock", true), ("other", false)]);
    let diagnostics =
        scenario.analyze("let consumer = other.Bucket { bucket = producer.new_name }\n");
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("got mock.Bucket.NewName"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
    let items = scenario
        .complete(
            "let consumer = other.Bucket {\n  bucket = \n}\n",
            Position::new(1, 11),
        )
        .await;
    assert!(
        !items.iter().any(|item| item.label == "producer.new_name"),
        "{items:#?}"
    );
    assert!(
        items.iter().any(|item| item.label == "producer.old_name"),
        "{items:#?}"
    );
}
