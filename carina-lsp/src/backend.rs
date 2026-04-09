use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_core::formatter::{self, FormatConfig};
use carina_core::parser::ProviderContext;
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
use crate::workspace;

/// Calculate the end position (line, character) of a text document.
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

/// Schema-dependent providers that are rebuilt when provider configs change.
struct ProviderState {
    diagnostic_engine: DiagnosticEngine,
    completion_provider: CompletionProvider,
    hover_provider: HoverProvider,
    semantic_tokens_provider: SemanticTokensProvider,
}

impl ProviderState {
    fn new(factories: Vec<Box<dyn ProviderFactory>>, provider_context: &ProviderContext) -> Self {
        let schemas = Arc::new(provider_mod::collect_schemas(&factories));
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
        let region_completions: Vec<CompletionValue> = factories
            .iter()
            .flat_map(|f| f.config_completions().remove("region").unwrap_or_default())
            .collect();
        let factories_arc = Arc::new(factories);
        Self {
            diagnostic_engine: DiagnosticEngine::new(
                Arc::clone(&schemas),
                provider_names.clone(),
                factories_arc,
            ),
            completion_provider: CompletionProvider::new(
                Arc::clone(&schemas),
                provider_names,
                region_completions.clone(),
                provider_context.validators.keys().cloned().collect(),
            ),
            semantic_tokens_provider: SemanticTokensProvider::new(&region_completions),
            hover_provider: HoverProvider::new(schemas, region_completions),
        }
    }
}

/// Function type for building provider factories from configs and a base directory.
/// This callback is provided by main.rs to keep provider-specific wiring out of the library.
pub type FactoryBuilder = Arc<
    dyn Fn(&[carina_core::parser::ProviderConfig], &Path) -> Vec<Box<dyn ProviderFactory>>
        + Send
        + Sync,
>;

pub struct Backend {
    client: Client,
    documents: DashMap<Url, Document>,
    providers: tokio::sync::RwLock<ProviderState>,
    provider_context: Arc<ProviderContext>,
    workspace_root: tokio::sync::OnceCell<Option<PathBuf>>,
    factory_builder: Option<FactoryBuilder>,
}

impl Backend {
    pub fn new(
        client: Client,
        provider_context: ProviderContext,
        factory_builder: Option<FactoryBuilder>,
    ) -> Self {
        let provider_context = Arc::new(provider_context);
        // Start with empty schemas — they will be loaded asynchronously after initialize
        let state = ProviderState::new(vec![], &provider_context);

        Self {
            client,
            documents: DashMap::new(),
            providers: tokio::sync::RwLock::new(state),
            provider_context,
            workspace_root: tokio::sync::OnceCell::new(),
            factory_builder,
        }
    }

    /// Returns the workspace root path, if available.
    pub fn workspace_root(&self) -> Option<&PathBuf> {
        self.workspace_root.get().and_then(|opt| opt.as_ref())
    }

    async fn update_diagnostics(&self, uri: Url) {
        if let Some(doc) = self.documents.get(&uri) {
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));

            let providers = self.providers.read().await;
            let diagnostics = providers
                .diagnostic_engine
                .analyze(&doc, base_path.as_deref());
            drop(providers);

            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
    }

    /// Load or reload provider schemas from workspace .crn files.
    async fn load_schemas(&self) {
        let workspace_root = match self.workspace_root() {
            Some(root) => root.clone(),
            None => return,
        };

        let factory_builder = match &self.factory_builder {
            Some(builder) => builder,
            None => return,
        };

        let provider_configs = workspace::discover_providers(&workspace_root);
        if provider_configs.is_empty() {
            // Clear schemas when no providers are configured
            *self.providers.write().await = ProviderState::new(vec![], &self.provider_context);
            let uris: Vec<Url> = self.documents.iter().map(|r| r.key().clone()).collect();
            for uri in uris {
                self.update_diagnostics(uri).await;
            }
            return;
        }

        // Build factories using the injected builder (runs WASM loading)
        let factories = tokio::task::spawn_blocking({
            let configs = provider_configs.clone();
            let dir = workspace_root.clone();
            let builder = Arc::clone(factory_builder);
            move || builder(&configs, &dir)
        })
        .await
        .unwrap_or_default();

        if factories.is_empty() {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!(
                        "Found {} provider(s) but no WASM binaries cached. Run `carina init` to download.",
                        provider_configs.len()
                    ),
                )
                .await;
            return;
        }

        let factory_count = factories.len();
        let new_state = ProviderState::new(factories, &self.provider_context);
        *self.providers.write().await = new_state;

        self.client
            .log_message(
                MessageType::INFO,
                format!("Loaded {} provider schema(s)", factory_count),
            )
            .await;

        // Re-run diagnostics on all open documents with new schemas
        let uris: Vec<Url> = self.documents.iter().map(|r| r.key().clone()).collect();
        for uri in uris {
            self.update_diagnostics(uri).await;
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
            .or_else(|| {
                params
                    .workspace_folders
                    .as_ref()
                    .and_then(|folders| folders.first())
                    .and_then(|f| f.uri.to_file_path().ok())
            });
        let _ = self.workspace_root.set(root);

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
        // Register file watcher for .crn files
        let registration = Registration {
            id: "crn-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![FileSystemWatcher {
                        glob_pattern: GlobPattern::String("**/*.crn".to_string()),
                        kind: Some(WatchKind::all()),
                    }],
                })
                .unwrap(),
            ),
        };
        let _ = self.client.register_capability(vec![registration]).await;

        self.client
            .log_message(MessageType::INFO, "Carina LSP server initialized")
            .await;

        // Load provider schemas asynchronously (doesn't block the event loop)
        self.load_schemas().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let doc = Document::new(
            params.text_document.text,
            Arc::clone(&self.provider_context),
        );
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

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let has_crn_changes = params
            .changes
            .iter()
            .any(|c| c.uri.as_str().ends_with(".crn"));

        if has_crn_changes {
            self.load_schemas().await;
        }
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        if let Some(doc) = self.documents.get(uri) {
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));

            let providers = self.providers.read().await;
            let completions =
                providers
                    .completion_provider
                    .complete(&doc, position, base_path.as_deref());
            return Ok(Some(CompletionResponse::Array(completions)));
        }
        Ok(None)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        if let Some(doc) = self.documents.get(uri) {
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));
            let providers = self.providers.read().await;
            return Ok(providers.hover_provider.hover_with_base_path(
                &doc,
                position,
                base_path.as_deref(),
            ));
        }
        Ok(None)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = &params.text_document.uri;

        if let Some(doc) = self.documents.get(uri) {
            let providers = self.providers.read().await;
            let tokens = providers.semantic_tokens_provider.tokenize(&doc.text());
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
                        return Ok(None);
                    }

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
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }
}
