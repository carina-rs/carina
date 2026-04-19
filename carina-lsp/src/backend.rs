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
    /// Configs and their source directory, retained so a background poller can
    /// re-probe provider installation without re-scanning the workspace.
    configs: Vec<(PathBuf, carina_core::parser::ProviderConfig)>,
    /// Injected install prober. Kept here so `is_stale()` can recompute the
    /// fingerprint with the same function that built the snapshot.
    prober: Option<ProviderInstallProber>,
    /// Snapshot of which providers resolved to an installed local binary when
    /// this state was built. Compared against a fresh probe to detect
    /// `.carina/` deletions the editor's file watcher did not report
    /// (issue #2023 follow-up: VS Code excludes dot-prefixed directories
    /// from its watcher by default).
    install_fingerprint: Vec<(String, bool)>,
}

impl ProviderState {
    fn new(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_errors: HashMap<String, String>,
        configs: Vec<(PathBuf, carina_core::parser::ProviderConfig)>,
        prober: Option<ProviderInstallProber>,
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
        let install_fingerprint = probe_install_fingerprint(prober.as_ref(), &configs);
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
            configs,
            prober,
            install_fingerprint,
        }
    }

    fn schema_count(&self) -> usize {
        self.diagnostic_engine.schema_count()
    }

    /// True when a fresh probe of the configured providers no longer matches
    /// the fingerprint captured at build time.
    fn is_stale(&self) -> bool {
        probe_install_fingerprint(self.prober.as_ref(), &self.configs) != self.install_fingerprint
    }
}

/// Checks whether a single provider config resolves to an installed local
/// binary. Injected from `main.rs` so provider-resolver calls stay out of
/// the provider-agnostic `carina-lsp` library code.
pub type ProviderInstallProber =
    Arc<dyn Fn(&Path, &carina_core::parser::ProviderConfig) -> bool + Send + Sync>;

/// Compute `(provider_name, is_installed)` pairs for a list of configs.
/// Ordered to match the input so equality comparisons are stable.
fn probe_install_fingerprint(
    prober: Option<&ProviderInstallProber>,
    configs: &[(PathBuf, carina_core::parser::ProviderConfig)],
) -> Vec<(String, bool)> {
    let Some(prober) = prober else {
        // Without a prober we can't tell, so claim every provider is still
        // installed — the poller falls back to a never-stale snapshot.
        return configs
            .iter()
            .map(|(_, cfg)| (cfg.name.clone(), true))
            .collect();
    };
    configs
        .iter()
        .map(|(dir, cfg)| (cfg.name.clone(), prober(dir, cfg)))
        .collect()
}

/// How often the background poller checks for `.carina/` drift. Short enough
/// that deleting the install feels interactive (~seconds), long enough that
/// the poll cost — one `fs::metadata` per configured provider — is
/// negligible.
const PROVIDER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-directory provider states keyed by configuration directory.
struct ProviderStates {
    /// Directory → ProviderState. Each directory with provider declarations
    /// gets its own state with its own schemas.
    by_dir: HashMap<PathBuf, ProviderState>,
    /// Reverse import map: module directory → list of caller directories.
    /// Used to resolve providers for module files that don't declare their own.
    import_map: HashMap<PathBuf, Vec<PathBuf>>,
    /// Fallback state for files that don't belong to any config directory.
    empty: ProviderState,
}

impl ProviderStates {
    fn new() -> Self {
        Self {
            by_dir: HashMap::new(),
            import_map: HashMap::new(),
            empty: ProviderState::new(vec![], HashMap::new(), vec![], None),
        }
    }

    /// Find the ProviderState for a given file path.
    ///
    /// 1. Walk up the directory tree to find the nearest config directory
    /// 2. If not found, check if the file's directory is imported by a caller
    ///    and use the caller's ProviderState
    fn state_for_path(&self, file_path: &Path) -> &ProviderState {
        // Start from file_path itself (which is the file's parent directory),
        // not file_path.parent() — the config dir might be the directory itself.
        let mut dir = Some(file_path);
        while let Some(d) = dir {
            if let Some(state) = self.by_dir.get(d) {
                return state;
            }
            // Try canonical path in case the by_dir key is different
            if let Ok(canonical) = d.canonicalize()
                && canonical != d
                && let Some(state) = self.by_dir.get(&canonical)
            {
                return state;
            }
            dir = d.parent();
        }

        // Check import map for module files
        let canonical = file_path.canonicalize().unwrap_or(file_path.to_path_buf());
        if let Some(callers) = self.import_map.get(&canonical) {
            for caller_dir in callers {
                let mut dir = Some(caller_dir.as_path());
                while let Some(d) = dir {
                    if let Some(state) = self.by_dir.get(d) {
                        return state;
                    }
                    dir = d.parent();
                }
            }
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
    documents: Arc<DashMap<Url, Document>>,
    providers: Arc<tokio::sync::RwLock<ProviderStates>>,
    provider_context: Arc<ProviderContext>,
    workspace_root: Arc<tokio::sync::OnceCell<Option<PathBuf>>>,
    factory_builder: Option<FactoryBuilder>,
    install_prober: Option<ProviderInstallProber>,
    /// Set once `initialized` spawns the background `.carina/` drift poller,
    /// to keep it from double-spawning on clients that re-send `initialized`.
    poller_spawned: std::sync::atomic::AtomicBool,
}

impl Backend {
    pub fn new(
        client: Client,
        provider_context: ProviderContext,
        factory_builder: Option<FactoryBuilder>,
    ) -> Self {
        Self::with_install_prober(client, provider_context, factory_builder, None)
    }

    /// Construct a backend with a custom install prober. `main.rs` uses this
    /// to inject a `carina_provider_resolver::find_installed_provider`-based
    /// prober without pulling the resolver into the library crate.
    pub fn with_install_prober(
        client: Client,
        provider_context: ProviderContext,
        factory_builder: Option<FactoryBuilder>,
        install_prober: Option<ProviderInstallProber>,
    ) -> Self {
        let provider_context = Arc::new(provider_context);

        Self {
            client,
            documents: Arc::new(DashMap::new()),
            providers: Arc::new(tokio::sync::RwLock::new(ProviderStates::new())),
            provider_context,
            workspace_root: Arc::new(tokio::sync::OnceCell::new()),
            factory_builder,
            install_prober,
            poller_spawned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns the workspace root path, if available.
    pub fn workspace_root(&self) -> Option<&PathBuf> {
        self.workspace_root.get().and_then(|opt| opt.as_ref())
    }

    /// Scan sibling `.crn` files in the same directory for `let` bindings
    /// and references to the current file's bindings.
    ///
    /// ## Scope
    ///
    /// Intentionally **one directory level only** (the parent directory of
    /// the file being edited). Carina is directory-scoped — each directory
    /// is its own parse unit. Files in nested subdirectories are separate
    /// modules or upstream projects with their own binding scopes, so they
    /// must not leak bindings into this cross-file reference check.
    /// Dedicated diagnostics
    /// ([`crate::diagnostics::checks::DiagnosticEngine::check_upstream_state_field_references`])
    /// handle references that do cross a directory boundary (upstream
    /// state exports, module call arguments) via
    /// `parse_directory_with_overrides`.
    ///
    /// ## Returns
    /// - `bindings`: binding_name → resource_type for `let` bindings in the
    ///   current file's sibling `.crn`s.
    /// - `referenced`: binding names from the current file that appear to be
    ///   referenced by those siblings.
    fn scan_sibling_context(
        dir: &Path,
        current_uri: &Url,
        current_bindings: &std::collections::HashSet<String>,
    ) -> (HashMap<String, String>, std::collections::HashSet<String>) {
        let mut bindings = HashMap::new();
        let mut referenced = std::collections::HashSet::new();
        let current_path = current_uri.to_file_path().ok();
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return (bindings, referenced),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "crn")
                && current_path.as_ref() != Some(&path)
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if let Some((binding, after_eq)) = crate::let_parse::parse_let_header(line) {
                        // Strip "read " prefix if present
                        let type_part = after_eq.strip_prefix("read ").unwrap_or(after_eq);
                        // Extract "provider.service.type" before "{"
                        if let Some(brace) = type_part.find('{') {
                            let resource_type = type_part[..brace].trim();
                            if resource_type.contains('.') {
                                bindings.insert(binding.to_string(), resource_type.to_string());
                            }
                        }
                    }

                    // Check if this line references any binding from the current file
                    // Look for "binding_name." patterns after "="
                    if let Some(eq_pos) = trimmed.find('=') {
                        let after_eq = trimmed[eq_pos + 1..].trim();
                        for name in current_bindings.iter() {
                            if after_eq.starts_with(&format!("{}.", name)) {
                                referenced.insert(name.clone());
                            }
                        }
                    }
                }
            }
        }
        (bindings, referenced)
    }

    async fn update_diagnostics(&self, uri: Url) {
        publish_diagnostics_for(&self.client, &self.documents, &self.providers, uri).await;
    }

    /// Spawn a background task that polls the on-disk install state for
    /// every loaded provider and triggers a reload when it diverges from the
    /// snapshot the LSP built from. This closes the gap that
    /// `workspace/didChangeWatchedFiles` leaves when the client excludes
    /// dot-prefixed directories like `.carina/` from its file watcher (the
    /// default in VS Code): deleting `.carina/` fires no event, so the
    /// factory stays live and the `not installed` diagnostic never returns.
    ///
    /// Called once from `initialized`. Subsequent calls are no-ops.
    fn spawn_provider_drift_poller(&self) {
        use std::sync::atomic::Ordering;
        if self.poller_spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let providers = Arc::clone(&self.providers);
        let documents = Arc::clone(&self.documents);
        let workspace_root = Arc::clone(&self.workspace_root);
        let factory_builder = self.factory_builder.as_ref().map(Arc::clone);
        let install_prober = self.install_prober.as_ref().map(Arc::clone);
        let client = self.client.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PROVIDER_POLL_INTERVAL);
            // Skip the initial tick — `initialize` already loaded schemas.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let any_stale = {
                    let guard = providers.read().await;
                    guard.by_dir.values().any(|s| s.is_stale())
                };
                if any_stale {
                    load_schemas_impl(
                        &client,
                        workspace_root.as_ref(),
                        factory_builder.as_ref(),
                        install_prober.as_ref(),
                        providers.as_ref(),
                        documents.as_ref(),
                    )
                    .await;
                }
            }
        });
    }

    /// Load or reload provider schemas from workspace .crn files.
    async fn load_schemas(&self) {
        load_schemas_impl(
            &self.client,
            self.workspace_root.as_ref(),
            self.factory_builder.as_ref(),
            self.install_prober.as_ref(),
            self.providers.as_ref(),
            self.documents.as_ref(),
        )
        .await;
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
        // Register file watchers for .crn files, provider WASMs, and lock files
        let registration = Registration {
            id: "crn-watcher".to_string(),
            method: "workspace/didChangeWatchedFiles".to_string(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: vec![
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String("**/*.crn".to_string()),
                            kind: Some(WatchKind::all()),
                        },
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String(
                                "**/.carina/providers/**/*.wasm".to_string(),
                            ),
                            kind: Some(WatchKind::all()),
                        },
                        FileSystemWatcher {
                            glob_pattern: GlobPattern::String(
                                "**/carina-providers.lock".to_string(),
                            ),
                            kind: Some(WatchKind::all()),
                        },
                    ],
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

        // Start polling for `.carina/` drift so a user deleting it mid-session
        // is noticed without any editor interaction.
        self.spawn_provider_drift_poller();
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
        let version = params.text_document.version;
        if let Some(mut doc) = self.documents.get_mut(&uri) {
            // Skip stale changes (older version than current)
            if version <= doc.version() {
                return;
            }
            for change in params.content_changes {
                doc.apply_change(change);
            }
            doc.set_version(version);
        }
        self.update_diagnostics(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        if should_reload_providers(&params.changes) {
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

/// Publish diagnostics for a single document. Free function so both the
/// `Backend` methods and the provider-drift poller can call it without going
/// through `&self`.
async fn publish_diagnostics_for(
    client: &Client,
    documents: &DashMap<Url, Document>,
    providers: &tokio::sync::RwLock<ProviderStates>,
    uri: Url,
) {
    let Some(doc) = documents.get(&uri) else {
        return;
    };
    let base_path = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    let current_bindings: std::collections::HashSet<String> = {
        let text = doc.text();
        let mut bindings = std::collections::HashSet::new();
        for line in text.lines() {
            if let Some((name, _)) = crate::let_parse::parse_let_header(line) {
                bindings.insert(name.to_string());
            }
        }
        bindings
    };

    let (sibling_bindings, sibling_referenced) = base_path
        .as_ref()
        .map(|dir| Backend::scan_sibling_context(dir, &uri, &current_bindings))
        .unwrap_or_default();

    let current_file_name: Option<String> = uri
        .to_file_path()
        .ok()
        .and_then(|p| p.file_name().and_then(|n| n.to_str().map(String::from)));

    let guard = providers.read().await;
    let state = base_path
        .as_ref()
        .map(|p| guard.state_for_path(p))
        .unwrap_or(&guard.empty);
    let diagnostics = state.diagnostic_engine.analyze_with_filename(
        &doc,
        current_file_name.as_deref(),
        base_path.as_deref(),
        &sibling_bindings,
        &sibling_referenced,
    );
    drop(guard);

    client.publish_diagnostics(uri, diagnostics, None).await;
}

/// Workspace-wide schema load/reload. Free function so the drift poller can
/// invoke it without holding `&Backend`.
async fn load_schemas_impl(
    client: &Client,
    workspace_root: &tokio::sync::OnceCell<Option<PathBuf>>,
    factory_builder: Option<&FactoryBuilder>,
    install_prober: Option<&ProviderInstallProber>,
    providers: &tokio::sync::RwLock<ProviderStates>,
    documents: &DashMap<Url, Document>,
) {
    let Some(Some(root)) = workspace_root.get() else {
        return;
    };
    let workspace_root = root.clone();

    let Some(factory_builder) = factory_builder else {
        return;
    };

    let dir_providers = workspace::discover_providers_by_dir(&workspace_root);
    let import_map = workspace::discover_import_map(&workspace_root);

    if dir_providers.is_empty() {
        let mut states = ProviderStates::new();
        states.import_map = import_map;
        *providers.write().await = states;
        let uris: Vec<Url> = documents.iter().map(|r| r.key().clone()).collect();
        for uri in uris {
            publish_diagnostics_for(client, documents, providers, uri).await;
        }
        return;
    }

    let mut states = ProviderStates::new();
    let mut total_schemas = 0;

    for (dir, configs) in &dir_providers {
        let dir_configs: Vec<(PathBuf, carina_core::parser::ProviderConfig)> =
            configs.iter().map(|c| (dir.clone(), c.clone())).collect();

        let (dir_factories, dir_errors) = tokio::task::spawn_blocking({
            let configs = dir_configs.clone();
            let builder = Arc::clone(factory_builder);
            move || builder(&configs)
        })
        .await
        .unwrap_or_default();

        for (name, reason) in &dir_errors {
            client
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

        let state = ProviderState::new(
            dir_factories,
            dir_errors,
            dir_configs,
            install_prober.cloned(),
        );
        total_schemas += state.schema_count();
        states.by_dir.insert(dir.clone(), state);
    }

    states.import_map = import_map;
    let dir_count = states.by_dir.len();
    *providers.write().await = states;
    client
        .log_message(
            MessageType::INFO,
            format!(
                "Loaded providers for {} directory(s), {} resource type schema(s) total",
                dir_count, total_schemas
            ),
        )
        .await;

    let uris: Vec<Url> = documents.iter().map(|r| r.key().clone()).collect();
    for uri in uris {
        publish_diagnostics_for(client, documents, providers, uri).await;
    }
}

/// Decide whether a batch of watched-file events requires rebuilding provider
/// factories. Split out so it can be unit tested without an LSP client.
fn should_reload_providers(changes: &[FileEvent]) -> bool {
    changes.iter().any(should_reload_for_event)
}

fn should_reload_for_event(event: &FileEvent) -> bool {
    let uri = event.uri.as_str();

    // A provider binary, its lock file, or any `.crn` file changing — create,
    // change, or delete — triggers a reload. Gating `.crn` changes on "file
    // currently declares a provider block" misses the removal case: editing a
    // file to delete its `provider NAME {}` block silently keeps the stale
    // factory live, because the post-save content no longer matches.
    // `didChangeWatchedFiles` fires on save / delete (not on `did_change`
    // keystrokes), so an unconditional reload cost is bounded and matches the
    // existing wasm / lock-file behavior.
    uri.ends_with(".wasm") || uri.ends_with("carina-providers.lock") || uri.ends_with(".crn")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_event(path: &std::path::Path, typ: FileChangeType) -> FileEvent {
        FileEvent {
            uri: Url::from_file_path(path).unwrap(),
            typ,
        }
    }

    #[test]
    fn reload_for_wasm_change_including_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let wasm = tmp.path().join("awscc.wasm");
        for typ in [
            FileChangeType::CREATED,
            FileChangeType::CHANGED,
            FileChangeType::DELETED,
        ] {
            assert!(
                should_reload_providers(&[file_event(&wasm, typ)]),
                "wasm {typ:?} must trigger reload"
            );
        }
    }

    #[test]
    fn reload_for_lock_file_change_including_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("carina-providers.lock");
        for typ in [
            FileChangeType::CREATED,
            FileChangeType::CHANGED,
            FileChangeType::DELETED,
        ] {
            assert!(
                should_reload_providers(&[file_event(&lock, typ)]),
                "lock {typ:?} must trigger reload"
            );
        }
    }

    #[test]
    fn reload_for_any_crn_change_including_removed_provider_block() {
        // Covers three scenarios in one:
        // - .crn that declares a provider block (adding `source = ...`)
        // - .crn whose previously-declared provider block was deleted on save
        //   (reviewer-spotted regression: gating on current content misses it)
        // - .crn that never declared a provider (`main.crn` edits).
        // All three must reload: we can't reliably detect the middle case
        // from the post-save content alone, and the reload cost is bounded
        // since `didChangeWatchedFiles` fires on save, not per-keystroke.
        let tmp = tempfile::tempdir().unwrap();
        for (name, content) in [
            (
                "with_block.crn",
                "provider awscc {\n  source = 'github.com/carina-rs/carina-provider-awscc'\n}\n",
            ),
            ("block_removed.crn", "# provider block removed\n"),
            ("main.crn", "awscc.s3.bucket { bucket_name = 'ex' }\n"),
        ] {
            let path = tmp.path().join(name);
            std::fs::write(&path, content).unwrap();
            for typ in [FileChangeType::CREATED, FileChangeType::CHANGED] {
                assert!(
                    should_reload_providers(&[file_event(&path, typ)]),
                    "{name} {typ:?} must trigger reload"
                );
            }
        }
    }

    #[test]
    fn reload_when_crn_file_is_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gone.crn");
        assert!(should_reload_providers(&[file_event(
            &path,
            FileChangeType::DELETED
        )]));
    }

    #[test]
    fn no_reload_for_unrelated_files() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["README.md", "Cargo.toml", "notes.txt"] {
            let path = tmp.path().join(name);
            assert!(
                !should_reload_providers(&[file_event(&path, FileChangeType::CHANGED)]),
                "{name} should not trigger reload"
            );
        }
    }

    /// Stub prober for tests: reports a provider as installed iff the
    /// attribute `_installed_at` points at an existing file. Lets us
    /// exercise the fingerprint logic without pulling the resolver crate.
    fn test_prober() -> ProviderInstallProber {
        Arc::new(|_dir, cfg| {
            cfg.attributes
                .get("_installed_at")
                .and_then(|v| match v {
                    carina_core::resource::Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .map(|p| std::path::Path::new(p).exists())
                .unwrap_or(false)
        })
    }

    /// The background drift poller relies on this flipping when `.carina/`
    /// is deleted out from under the LSP. Tests the invariant directly:
    /// build an install, probe (installed), delete it, probe again (not
    /// installed). If these two results are equal the poller will never
    /// fire and a VS Code user deleting `.carina/` mid-session will keep
    /// seeing stale diagnostics until they save a `.crn`.
    #[test]
    fn install_fingerprint_flips_when_local_wasm_is_deleted() {
        use carina_core::parser::ProviderConfig;
        use carina_core::resource::Value;

        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("carina-provider-foo.wasm");
        std::fs::write(&installed, b"fake").unwrap();

        let mut attributes = std::collections::HashMap::new();
        attributes.insert(
            "_installed_at".to_string(),
            Value::String(installed.display().to_string()),
        );
        let config = ProviderConfig {
            name: "foo".into(),
            source: Some("github.com/carina-rs/stub".into()),
            version: None,
            revision: None,
            attributes,
            default_tags: std::collections::HashMap::new(),
        };
        let configs = vec![(tmp.path().to_path_buf(), config)];
        let prober = test_prober();

        let before = probe_install_fingerprint(Some(&prober), &configs);
        assert_eq!(
            before,
            vec![("foo".to_string(), true)],
            "initial probe should see the installed binary"
        );

        std::fs::remove_file(&installed).unwrap();
        let after = probe_install_fingerprint(Some(&prober), &configs);
        assert_eq!(
            after,
            vec![("foo".to_string(), false)],
            "probe after delete must flip to false"
        );
        assert_ne!(before, after);
    }

    #[test]
    fn install_fingerprint_stable_when_nothing_changes() {
        use carina_core::parser::ProviderConfig;
        let tmp = tempfile::tempdir().unwrap();
        let config = ProviderConfig {
            name: "missing".into(),
            source: None,
            version: None,
            revision: None,
            attributes: std::collections::HashMap::new(),
            default_tags: std::collections::HashMap::new(),
        };
        let configs = vec![(tmp.path().to_path_buf(), config)];
        let prober = test_prober();
        assert_eq!(
            probe_install_fingerprint(Some(&prober), &configs),
            probe_install_fingerprint(Some(&prober), &configs),
            "two back-to-back probes with no fs change must agree"
        );
    }
}
