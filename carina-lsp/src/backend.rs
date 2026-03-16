use std::sync::Arc;

use carina_core::formatter::{self, FormatConfig};
use carina_core::provider::{self as provider_mod, ProviderFactory};
use carina_core::schema::CompletionValue;
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::completion::CompletionProvider;
use crate::diagnostics::DiagnosticEngine;
use crate::document::Document;
use crate::hover::HoverProvider;
use crate::semantic_tokens::{self, SemanticTokensProvider};

/// Calculate the end position (line, character) of a text document.
/// Returns (last_line, last_character) using character counts (not byte lengths).
pub fn document_end_position(text: &str) -> (u32, u32) {
    let line_count = text.chars().filter(|&c| c == '\n').count();
    let last_line = line_count as u32;
    let last_char = text
        .lines()
        .last()
        .map(|l| l.chars().count() as u32)
        .unwrap_or(0);
    let last_char = if text.ends_with('\n') { 0 } else { last_char };
    (last_line, last_char)
}

pub struct Backend {
    client: Client,
    documents: DashMap<Url, Document>,
    diagnostic_engine: DiagnosticEngine,
    completion_provider: CompletionProvider,
    hover_provider: HoverProvider,
    semantic_tokens_provider: SemanticTokensProvider,
}

impl Backend {
    pub fn new(client: Client, factories: Vec<Box<dyn ProviderFactory>>) -> Self {
        // Build shared schema map from all factories
        let schemas = Arc::new(provider_mod::collect_schemas(&factories));

        // Collect provider names
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();

        // Collect region completions from all factories
        let region_completions: Vec<CompletionValue> = factories
            .iter()
            .flat_map(|f| f.region_completions())
            .collect();

        // Wrap factories in Arc for sharing
        let factories: Arc<Vec<Box<dyn ProviderFactory>>> = Arc::new(factories);

        Self {
            client,
            documents: DashMap::new(),
            diagnostic_engine: DiagnosticEngine::new(
                Arc::clone(&schemas),
                provider_names.clone(),
                Arc::clone(&factories),
            ),
            completion_provider: CompletionProvider::new(
                Arc::clone(&schemas),
                provider_names.clone(),
                region_completions.clone(),
            ),
            semantic_tokens_provider: SemanticTokensProvider::new(&region_completions),
            hover_provider: HoverProvider::new(Arc::clone(&schemas), region_completions),
        }
    }

    async fn update_diagnostics(&self, uri: Url) {
        if let Some(doc) = self.documents.get(&uri) {
            // Get base path from URI for module resolution
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));

            let diagnostics = self.diagnostic_engine.analyze(&doc, base_path.as_deref());
            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        "=".to_string(),
                        " ".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic_tokens::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            work_done_progress_options: Default::default(),
                        },
                    ),
                ),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Carina LSP server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let doc = Document::new(params.text_document.text);
        self.documents.insert(uri.clone(), doc);
        self.update_diagnostics(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        if let Some(mut doc) = self.documents.get_mut(&uri) {
            for change in params.content_changes {
                doc.apply_change(change);
            }
        }
        self.update_diagnostics(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        if let Some(doc) = self.documents.get(uri) {
            // Get base path from URI for module resolution
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));

            let completions =
                self.completion_provider
                    .complete(&doc, position, base_path.as_deref());
            return Ok(Some(CompletionResponse::Array(completions)));
        }
        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        if let Some(doc) = self.documents.get(uri) {
            return Ok(self.hover_provider.hover(&doc, position));
        }
        Ok(None)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        if let Some(doc) = self.documents.get(uri) {
            let tokens = self.semantic_tokens_provider.tokenize(&doc.text());
            return Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: tokens,
            })));
        }
        Ok(None)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = &params.text_document.uri;

        if let Some(doc) = self.documents.get(uri) {
            let text = doc.text();
            let config = FormatConfig::default();

            match formatter::format(&text, &config) {
                Ok(formatted) => {
                    if formatted == text {
                        // No changes needed
                        return Ok(None);
                    }

                    // Calculate the range covering the entire document
                    let (last_line, last_char) = document_end_position(&text);

                    let edit = TextEdit {
                        range: Range {
                            start: Position {
                                line: 0,
                                character: 0,
                            },
                            end: Position {
                                line: last_line,
                                character: last_char,
                            },
                        },
                        new_text: formatted,
                    };

                    return Ok(Some(vec![edit]));
                }
                Err(_) => {
                    // Formatting failed, return no edits
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_end_position_ascii() {
        let text = "line1\nline2\nline3";
        let (line, char) = document_end_position(text);
        assert_eq!(line, 2);
        assert_eq!(char, 5); // "line3" is 5 chars
    }

    #[test]
    fn test_document_end_position_trailing_newline() {
        let text = "line1\nline2\n";
        let (line, char) = document_end_position(text);
        assert_eq!(line, 2);
        assert_eq!(char, 0); // ends with newline, last line is empty
    }

    #[test]
    fn test_document_end_position_non_ascii_last_line() {
        // Last line contains Japanese characters (3 bytes each in UTF-8)
        // "あいう" = 3 chars but 9 bytes
        let text = "line1\nあいう";
        let (line, char) = document_end_position(text);
        assert_eq!(line, 1);
        assert_eq!(
            char, 3,
            "Should count characters (3), not bytes (9) for non-ASCII last line"
        );
    }

    #[test]
    fn test_document_end_position_empty() {
        let text = "";
        let (line, char) = document_end_position(text);
        assert_eq!(line, 0);
        assert_eq!(char, 0);
    }

    #[test]
    fn test_document_end_position_mixed_content() {
        // "// コメント" on last line: 2 + 1 + 5 = 8 chars, but 2 + 1 + 15 = 18 bytes
        let text = "aws.s3.bucket {\n    name = \"テスト\"\n// コメント";
        let (line, char) = document_end_position(text);
        assert_eq!(line, 2);
        assert_eq!(
            char,
            "// コメント".chars().count() as u32,
            "Should use character count for mixed ASCII/non-ASCII"
        );
    }
}
