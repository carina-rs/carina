use std::collections::HashMap;
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
    fn new(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_errors: HashMap<String, String>,
    ) -> Self {
        let schemas = Arc::new(provider_mod::collect_schemas(&factories));
        let provider_names: Vec<String> = factories.iter().map(|f| f.name().to_string()).collect();
        let region_completions: Vec<CompletionValue> = factories
            .iter()
            .flat_map(|f| f.config_completions().remove("region").unwrap_or_default())
            .collect();
        // Extract custom type names from provider schemas for completion
        let custom_type_names = provider_mod::collect_custom_type_names(&schemas);
        let factories_arc = Arc::new(factories);
        Self {
            diagnostic_engine: DiagnosticEngine::new(
                Arc::clone(&schemas),
                provider_names.clone(),
                factories_arc,
            )
            .with_provider_errors(provider_errors),
            completion_provider: CompletionProvider::new(
                Arc::clone(&schemas),
                provider_names,
                region_completions.clone(),
                custom_type_names,
            ),
            semantic_tokens_provider: SemanticTokensProvider::new(&region_completions),
            hover_provider: HoverProvider::new(schemas, region_completions),
        }
    }

    fn schema_count(&self) -> usize {
        self.diagnostic_engine.schema_count()
    }
}

/// Per-directory provider states keyed by configuration directory.
struct ProviderStates {
    /// Directory → ProviderState. Each directory with provider declarations
    /// gets its own state with its own schemas.
    by_dir: HashMap<PathBuf, ProviderState>,
    /// Fallback state for files that don't belong to any config directory.
    empty: ProviderState,
}

impl ProviderStates {
    fn new() -> Self {
        Self {
            by_dir: HashMap::new(),
            empty: ProviderState::new(vec![], HashMap::new()),
        }
    }

    /// Find the ProviderState for a given file path by walking up the
    /// directory tree to find the nearest config directory.
    fn state_for_path(&self, file_path: &Path) -> &ProviderState {
        let mut dir = file_path.parent();
        while let Some(d) = dir {
            if let Some(state) = self.by_dir.get(d) {
                return state;
            }
            dir = d.parent();
        }
        &self.empty
    }
}

/// Result of building provider factories: loaded factories + per-provider error messages.
pub type FactoryBuildResult = (
    Vec<Box<dyn ProviderFactory>>,
    HashMap<String, String>, // provider name -> error reason
);

/// Function type for building provider factories from configs with their source directories.
/// Each tuple is (source_directory, provider_config). The source directory is where the
/// `.crn` file defining the provider was found, used for installing providers in the
/// correct location rather than at the workspace root.
pub type FactoryBuilder = Arc<
    dyn Fn(&[(PathBuf, carina_core::parser::ProviderConfig)]) -> FactoryBuildResult + Send + Sync,
>;

pub struct Backend {
    client: Client,
    documents: DashMap<Url, Document>,
    providers: tokio::sync::RwLock<ProviderStates>,
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

        Self {
            client,
            documents: DashMap::new(),
            providers: tokio::sync::RwLock::new(ProviderStates::new()),
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
            let state = base_path
                .as_ref()
                .map(|p| providers.state_for_path(p))
                .unwrap_or(&providers.empty);
            let diagnostics = state.diagnostic_engine.analyze(&doc, base_path.as_deref());
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

        let dir_providers = workspace::discover_providers_by_dir(&workspace_root);
        if dir_providers.is_empty() {
            *self.providers.write().await = ProviderStates::new();
            let uris: Vec<Url> = self.documents.iter().map(|r| r.key().clone()).collect();
            for uri in uris {
                self.update_diagnostics(uri).await;
            }
            return;
        }

        // Build factories per directory. Each directory gets its own set of
        // provider factories loaded from its own provider configs.
        // WASM loading is cached on disk, so repeated loads are fast.
        let mut states = ProviderStates::new();
        let mut total_schemas = 0;

        for (dir, configs) in &dir_providers {
            let dir_configs: Vec<(PathBuf, carina_core::parser::ProviderConfig)> =
                configs.iter().map(|c| (dir.clone(), c.clone())).collect();

            let (dir_factories, dir_errors) = tokio::task::spawn_blocking({
                let configs = dir_configs;
                let builder = Arc::clone(factory_builder);
                move || builder(&configs)
            })
            .await
            .unwrap_or_default();

            for (name, reason) in &dir_errors {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        format!(
                            "Provider '{}' not loaded in {}: {}",
                            name,
                            dir.display(),
                            reason
                        ),
                    )
                    .await;
            }

            let state = ProviderState::new(dir_factories, dir_errors);
            total_schemas += state.schema_count();
            states.by_dir.insert(dir.clone(), state);
        }

        let dir_count = states.by_dir.len();
        *self.providers.write().await = states;

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Loaded providers for {} directory(s), {} resource type schema(s) total",
                    dir_count, total_schemas
                ),
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
            let state = base_path
                .as_ref()
                .map(|p| providers.state_for_path(p))
                .unwrap_or(&providers.empty);
            let completions =
                state
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
            let state = base_path
                .as_ref()
                .map(|p| providers.state_for_path(p))
                .unwrap_or(&providers.empty);
            return Ok(state.hover_provider.hover_with_base_path(
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
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));
            let providers = self.providers.read().await;
            let state = base_path
                .as_ref()
                .map(|p| providers.state_for_path(p))
                .unwrap_or(&providers.empty);
            let tokens = state.semantic_tokens_provider.tokenize(&doc.text());
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
