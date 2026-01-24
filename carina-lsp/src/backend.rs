use carina_core::formatter::{self, FormatConfig};
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::completion::CompletionProvider;
use crate::diagnostics::DiagnosticEngine;
use crate::document::Document;
use crate::hover::HoverProvider;
use crate::semantic_tokens::{self, SemanticTokensProvider};

pub struct Backend {
    client: Client,
    documents: DashMap<Url, Document>,
    diagnostic_engine: DiagnosticEngine,
    completion_provider: CompletionProvider,
    hover_provider: HoverProvider,
    semantic_tokens_provider: SemanticTokensProvider,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: DashMap::new(),
            diagnostic_engine: DiagnosticEngine::new(),
            completion_provider: CompletionProvider::new(),
            hover_provider: HoverProvider::new(),
            semantic_tokens_provider: SemanticTokensProvider::new(),
        }
    }

    async fn update_diagnostics(&self, uri: Url) {
        if let Some(doc) = self.documents.get(&uri) {
            let diagnostics = self.diagnostic_engine.analyze(&doc);
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
            let completions = self.completion_provider.complete(&doc, position);
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
                    let lines: Vec<&str> = text.lines().collect();
                    let last_line = lines.len().saturating_sub(1) as u32;
                    let last_char = lines.last().map(|l| l.len() as u32).unwrap_or(0);

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
