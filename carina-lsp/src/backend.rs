use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use carina_core::formatter::{self, FormatConfig};
use carina_core::parser::ProviderContext;
use carina_core::paths::PathIdentity;
use carina_core::provider::{self as provider_mod, ProviderFactory};
use carina_core::schema::{CompletionValue, SchemaRegistry, collect_all_block_names};
use carina_provider_resolver::{LegacyLockEntryError, ProviderArtifactFingerprint};
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

fn format_document_with_schemas(
    text: &str,
    config: &FormatConfig,
    schemas: &SchemaRegistry,
) -> std::result::Result<String, formatter::FormatParseError> {
    let block_names = collect_all_block_names(schemas);
    formatter::format_with_block_names(text, config, &block_names)
}

/// Schema-dependent providers that are rebuilt when provider configs change.
struct ProviderState {
    schemas: Arc<SchemaRegistry>,
    diagnostic_engine: DiagnosticEngine,
    completion_provider: CompletionProvider,
    hover_provider: HoverProvider,
    semantic_tokens_provider: SemanticTokensProvider,
    /// Configs and their source directory, retained so a background poller can
    /// re-probe provider installation without re-scanning the workspace.
    configs: Vec<(PathBuf, workspace::DiscoveredProviderConfig)>,
    /// Injected install prober. Kept here so `is_stale()` can recompute the
    /// fingerprint with the same function that built the snapshot.
    prober: Option<ProviderInstallProber>,
    /// Snapshot of the selected artifacts and lock files when this state was
    /// built. Compared against a fresh probe to detect replacements or
    /// `.carina/` deletions the editor's file watcher did not report
    /// (issue #2023 follow-up: VS Code excludes dot-prefixed directories
    /// from its watcher by default).
    install_fingerprint: Vec<(String, ProviderInstallFingerprint)>,
}

impl ProviderState {
    fn new(
        factories: Vec<Box<dyn ProviderFactory>>,
        provider_errors: HashMap<String, String>,
        configs: Vec<(PathBuf, workspace::DiscoveredProviderConfig)>,
        prober: Option<ProviderInstallProber>,
        install_fingerprint: Vec<(String, ProviderInstallFingerprint)>,
    ) -> Self {
        let use_builtin_mock = carina_provider_mock::uses_builtin_provider(
            configs.iter().map(|(_, config)| config.factory_config()),
            |provider_name| {
                factories
                    .iter()
                    .any(|factory| factory.name() == provider_name)
            },
        );
        let mut provider_names: Vec<String> =
            factories.iter().map(|f| f.name().to_string()).collect();
        let mut known_provider_names = HashSet::new();
        let mut supplemental_schemas = SchemaRegistry::new();
        if use_builtin_mock {
            supplemental_schemas
                .insert_provider_witness(
                    carina_provider_mock::BUILTIN_PROVIDER_NAME,
                    &carina_provider_mock::builtin_schema_witness(),
                )
                .expect("the built-in mock exposes one consistent schema witness");
            provider_names.push(carina_provider_mock::BUILTIN_PROVIDER_NAME.to_string());
            known_provider_names.insert(carina_provider_mock::BUILTIN_PROVIDER_NAME.to_string());
        }
        let region_completions: Vec<CompletionValue> = factories
            .iter()
            .flat_map(|f| f.config_completions().remove("region").unwrap_or_default())
            .collect();
        let factories = Arc::new(factories);
        let (schemas, diagnostic_engine) = if provider_names.is_empty() {
            (
                Arc::new(SchemaRegistry::new()),
                DiagnosticEngine::without_loaded_provider_schemas(),
            )
        } else {
            let loaded_providers = Arc::new(provider_mod::loaded_provider_snapshot_from_factories(
                Arc::clone(&factories),
                supplemental_schemas,
            ));
            let schemas = loaded_providers.schemas().schemas_arc();
            let engine = DiagnosticEngine::new_with_known_providers(
                loaded_providers,
                provider_names.clone(),
                known_provider_names,
            );
            (schemas, engine)
        };
        // Extract custom type names from provider schemas for completion.
        // The structured identities render to their dotted display form
        // (`aws.iam.Role.Arn`, `Ipv4Cidr`), which is also the spelling a
        // user writes in a type annotation.
        let custom_type_names: Vec<String> = provider_mod::collect_custom_type_names(&schemas)
            .iter()
            .map(|id| id.to_string())
            .collect();
        let diagnostic_engine = diagnostic_engine.with_provider_errors(provider_errors);
        Self {
            schemas: Arc::clone(&schemas),
            diagnostic_engine,
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
        let Some(prober) = self.prober.as_ref() else {
            return false;
        };
        probe_install_fingerprint(prober, &self.configs) != self.install_fingerprint
    }
}

/// Captures cheap metadata for a config's selected artifact and lock file.
/// Injected from `main.rs` so the poller and factory builder share resolver
/// path-selection behavior.
pub type ProviderInstallProber = Arc<
    dyn Fn(&Path, &carina_core::parser::ProviderConfig) -> ProviderInstallFingerprint + Send + Sync,
>;

pub type ProviderInstallFingerprint =
    std::result::Result<ProviderArtifactFingerprint, LegacyLockEntryError>;

/// Compute `(provider_name, metadata)` pairs for discovered configs.
/// Ordered to match the input so equality comparisons are stable.
fn probe_install_fingerprint(
    prober: &ProviderInstallProber,
    configs: &[(PathBuf, workspace::DiscoveredProviderConfig)],
) -> Vec<(String, ProviderInstallFingerprint)> {
    configs
        .iter()
        .map(|(dir, cfg)| {
            let cfg = cfg.factory_config();
            (cfg.name.clone(), prober(dir, cfg))
        })
        .collect()
}

/// How often the background poller checks for `.carina/` drift. Short enough
/// that replacing an install feels interactive (~seconds), long enough that
/// the poll cost — selected-artifact and lock-file metadata — is negligible.
const PROVIDER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-directory provider states keyed by configuration directory.
struct ProviderStates {
    /// Directory → ProviderState. Each directory with provider declarations
    /// gets its own state with its own schemas.
    by_dir: HashMap<PathIdentity, ProviderState>,
    /// Reverse import map: module directory → sorted set of caller directories.
    /// Used to resolve providers for module files that don't declare their own.
    import_map: workspace::ModuleImportMap,
    /// Fallback state for files that don't belong to any config directory.
    empty: ProviderState,
}

impl ProviderStates {
    fn new() -> Self {
        Self {
            by_dir: HashMap::new(),
            import_map: HashMap::new(),
            empty: ProviderState::new(vec![], HashMap::new(), vec![], None, vec![]),
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
            if let Some(state) = self.by_dir.get(&PathIdentity::new(d)) {
                return state;
            }
            dir = d.parent();
        }

        // Check import map for module files
        let canonical = PathIdentity::new(file_path);
        if let Some(callers) = self.import_map.get(&canonical) {
            for caller_dir in callers {
                for identity in caller_dir.ancestors() {
                    if let Some(state) = self.by_dir.get(&identity) {
                        return state;
                    }
                }
            }
        }

        &self.empty
    }
}

/// Result of building provider factories: loaded factories, per-provider
/// error messages, and the `(name, metadata)` observations the builder
/// made. The fingerprint is returned here (rather than recomputed) so the
/// stored factories and the drift-poll baseline describe the exact same
/// filesystem snapshot — any later divergence is a real change, not a
/// TOCTOU artifact or a file://-vs-`.carina/` mismatch.
pub type FactoryBuildResult = (
    Vec<Box<dyn ProviderFactory>>,
    HashMap<String, String>, // provider name -> error reason
    Vec<(String, ProviderInstallFingerprint)>, // install fingerprint
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

    /// Construct a backend with a custom install prober. `main.rs` injects the
    /// shared `carina_provider_resolver::provider_artifact_fingerprint` so the
    /// factory snapshot and background poll use the same probe.
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
                        // `/` re-triggers path completion inside `use { source = '...' }`
                        // so typing `../` fetches the parent dir's entries without
                        // requiring a manual Ctrl-Space. Harmless outside import paths:
                        // other contexts return nothing for a bare `/`.
                        "/".to_string(),
                        // `{` opens an interpolation expression (`${`) inside a
                        // double-quoted string. Without it the editor never asks the
                        // server for completion at the empty interpolation position,
                        // so in-scope binding names (`orgs`, `bucket`, …) wouldn't
                        // surface until the user typed an identifier char or hit
                        // Ctrl-Space manually.
                        "{".to_string(),
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
                // Declare `code_action_kinds` explicitly so editors that
                // gate on `only` (`CodeActionContext.only`) can avoid
                // probing this server for kinds it never emits. Carina
                // only ships QUICKFIX actions for #2309 today; add more
                // kinds here when other features start emitting actions.
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        resolve_provider: None,
                        work_done_progress_options: Default::default(),
                    },
                )),
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
        let Ok(source_file) = uri.to_file_path() else {
            self.client
                .log_message(
                    MessageType::WARNING,
                    format!("Cannot analyze non-file document URI: {uri}"),
                )
                .await;
            return;
        };
        let doc = Document::new_with_path(
            params.text_document.text,
            source_file,
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
            let current_file_name: Option<String> = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.file_name().and_then(|n| n.to_str().map(String::from)));
            let providers = self.providers.read().await;
            let state = base_path
                .as_ref()
                .map(|p| providers.state_for_path(p))
                .unwrap_or(&providers.empty);
            return Ok(state.hover_provider.hover_with_context(
                &doc,
                position,
                base_path.as_deref(),
                current_file_name.as_deref(),
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
            let base_path = uri
                .to_file_path()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()));
            let schemas = {
                let providers = self.providers.read().await;
                let state = base_path
                    .as_ref()
                    .map(|p| providers.state_for_path(p))
                    .unwrap_or(&providers.empty);
                Arc::clone(&state.schemas)
            };

            match format_document_with_schemas(&text, &config, &schemas) {
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

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        // The handler is purely diagnostic-driven: the editor passes the
        // diagnostics it currently shows in `params.context.diagnostics`,
        // each one carrying its `Diagnostic.data` payload unchanged. We
        // read the structured `EnumDiagnosticData` off matching
        // diagnostics and emit one `CodeAction` per remaining candidate.
        // Diagnostics without an enum payload (or from other features)
        // produce no actions here.
        let uri = &params.text_document.uri;
        let mut actions: Vec<CodeActionOrCommand> = Vec::new();
        for diag in &params.context.diagnostics {
            for action in crate::code_action::code_actions_for_diagnostic(uri, diag) {
                actions.push(CodeActionOrCommand::CodeAction(action));
            }
        }
        if actions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(actions))
        }
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
    let current_file_path = uri.to_file_path().ok();
    let base_path = current_file_path
        .as_deref()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    let guard = providers.read().await;
    let state = base_path
        .as_ref()
        .map(|p| guard.state_for_path(p))
        .unwrap_or(&guard.empty);
    let diagnostics = state
        .diagnostic_engine
        .analyze(&doc, current_file_path.as_deref());
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

    let mut dir_providers = HashMap::new();
    for (dir, configs) in workspace::discover_providers_by_dir(&workspace_root) {
        let entry = dir_providers
            .entry(PathIdentity::new(&dir))
            .or_insert_with(|| (dir.clone(), configs.clone()));
        // Discovery visits aliases separately; retain one stable display spelling.
        if dir < entry.0 {
            *entry = (dir, configs);
        }
    }
    let import_map = workspace::discover_import_map(&workspace_root);

    // Drain the current states up-front so matching per-dir entries can
    // be reused without re-running the factory builder. Anything left in
    // `old_states` at the end is dropped (e.g. a directory whose
    // provider block was deleted in this save). Reusing factories is the
    // fix for the RSS-grows-per-save pathology in #2136: every `.crn`
    // save fires this code, but the only work that must actually happen
    // is re-parsing when the provider configuration changed.
    let (mut old_states, old_import_map) = {
        let mut guard = providers.write().await;
        (
            std::mem::take(&mut guard.by_dir),
            std::mem::take(&mut guard.import_map),
        )
    };

    if dir_providers.is_empty() {
        let dropped_any = !old_states.is_empty();
        let mut states = ProviderStates::new();
        states.import_map = import_map.clone();
        *providers.write().await = states;
        if dropped_any || old_import_map != import_map {
            let uris: Vec<Url> = documents.iter().map(|r| r.key().clone()).collect();
            for uri in uris {
                publish_diagnostics_for(client, documents, providers, uri).await;
            }
        }
        return;
    }

    let mut states = ProviderStates::new();
    let mut total_schemas = 0;
    // Track whether any dir was freshly rebuilt (or whether the set of
    // known dirs changed). When every dir was reused and the import map
    // hasn't changed either, the whole load is a no-op and we can skip
    // the per-document diagnostic re-publish — that is the hot path for
    // the `.crn`-save → save-save-save loop in #2136.
    let mut any_work = false;

    for (identity, (dir, configs)) in dir_providers {
        let dir_configs: Vec<(PathBuf, workspace::DiscoveredProviderConfig)> =
            configs.iter().map(|c| (dir.clone(), c.clone())).collect();

        // Reuse the existing state when its cached configs are still the
        // current truth. Skips the WASM parse and all downstream schema /
        // completion / diagnostic scaffolding.
        if let Some(existing) = old_states.remove(&identity)
            && configs_match(&existing.configs, &dir_configs)
        {
            total_schemas += existing.schema_count();
            states.by_dir.insert(identity, existing);
            continue;
        }

        any_work = true;
        let (dir_factories, dir_errors, dir_fingerprint) = tokio::task::spawn_blocking({
            let configs = dir_configs
                .iter()
                .map(|(dir, config)| (dir.clone(), config.factory_config().clone()))
                .collect::<Vec<_>>();
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
            dir_fingerprint,
        );
        total_schemas += state.schema_count();
        states.by_dir.insert(identity, state);
    }

    // `old_states` was drained by `remove()` above; anything left is a
    // directory whose provider block was deleted, which counts as work
    // too (downstream diagnostics depend on it being gone).
    any_work |= !old_states.is_empty();
    // Import map changes don't affect factories but do shift which
    // `ProviderState` applies to module files; republish diagnostics in
    // that case even when factories were all reused.
    let import_map_changed = old_import_map != import_map;
    states.import_map = import_map;
    let dir_count = states.by_dir.len();
    *providers.write().await = states;

    if any_work {
        client
            .log_message(
                MessageType::INFO,
                format!(
                    "Loaded providers for {} directory(s), {} resource type schema(s) total",
                    dir_count, total_schemas
                ),
            )
            .await;
    }

    if any_work || import_map_changed {
        let uris: Vec<Url> = documents.iter().map(|r| r.key().clone()).collect();
        for uri in uris {
            publish_diagnostics_for(client, documents, providers, uri).await;
        }
    }
}

/// True when the two config lists describe the same set of providers
/// per directory, including parser-stage attribute values and default tags.
/// [`load_schemas_impl`] uses the retained discovery key, not the filtered
/// factory config, so a mid-edit placeholder still invalidates the cache.
/// `VersionConstraint` compares its raw DSL form, so two configs parsed from
/// the same source text compare equal even when their internal
/// `semver::VersionReq` representation differs.
fn configs_match(
    a: &[(PathBuf, workspace::DiscoveredProviderConfig)],
    b: &[(PathBuf, workspace::DiscoveredProviderConfig)],
) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((a_dir, a_config), (b_dir, b_config))| {
                PathIdentity::new(a_dir) == PathIdentity::new(b_dir)
                    && a_config.comparison_key() == b_config.comparison_key()
            })
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
    use carina_core::provider::{BoxFuture, ProviderResult};
    use carina_core::resource::Value;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    use indexmap::IndexMap;

    struct BlockSchemaFactory {
        block_name: &'static str,
    }

    impl ProviderFactory for BlockSchemaFactory {
        fn name(&self) -> &str {
            "mock"
        }

        fn display_name(&self) -> &str {
            "Mock"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            String::new()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
            _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
            Box::pin(async { unreachable!("formatting tests do not instantiate providers") })
        }

        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let rules = AttributeType::list(AttributeType::struct_(
                "Rule",
                vec![StructField::optional("action", AttributeType::string()).build()],
            ));
            vec![
                ResourceSchema::new("test.resource")
                    .attribute(
                        AttributeSchema::optional("rules", rules)
                            .with_block_name(self.block_name)
                            .build(),
                    )
                    .validate()
                    .expect("formatting test schema must be valid"),
            ]
        }
    }

    fn provider_state(block_name: &'static str) -> ProviderState {
        let config = carina_core::parser::parse_file(
            "provider mock {}\n",
            std::path::Path::new("providers.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .unwrap()
        .providers
        .into_iter()
        .next()
        .unwrap();
        let config = workspace::DiscoveredProviderConfig::from_parser(config);
        ProviderState::new(
            vec![Box::new(BlockSchemaFactory { block_name })],
            HashMap::new(),
            vec![(PathBuf::new(), config)],
            None,
            vec![],
        )
    }

    fn builtin_mock_provider_state(directory: &std::path::Path) -> ProviderState {
        let discovered = workspace::discover_providers_by_dir(directory);
        let configs = discovered[directory]
            .iter()
            .cloned()
            .map(|config| (directory.to_path_buf(), config))
            .collect();
        ProviderState::new(
            vec![],
            HashMap::new(),
            configs,
            None,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )],
        )
    }

    #[test]
    fn formatting_without_eligible_block_names_matches_plain_format() {
        let input = r#"awscc.ec2.ipam {
  description="regional IPAM"
  tags = ["network", "shared"]
}
"#;
        let config = FormatConfig::default();
        let schemas = SchemaRegistry::new();
        let expected = formatter::format(input, &config).unwrap();

        let actual = format_document_with_schemas(input, &config, &schemas).unwrap();

        assert_eq!(actual.as_bytes(), expected.as_bytes());
    }

    #[test]
    fn builtin_mock_diagnostics_match_validate_across_sibling_files() {
        let temp = tempfile::tempdir().unwrap();
        let providers = "provider mock {}\n";
        let main = r#"mock.unknown.Widget {}

mock.test.resource {}

mock.test.Thing {
  name = "alpha"
}

mock.test.Thing {
  name = "beta"
}
"#;
        std::fs::write(temp.path().join("providers.crn"), providers).unwrap();
        std::fs::write(temp.path().join("main.crn"), main).unwrap();

        let discovered = workspace::discover_providers_by_dir(temp.path());
        let configs = discovered[temp.path()]
            .iter()
            .cloned()
            .map(|config| (temp.path().to_path_buf(), config))
            .collect();
        let state = ProviderState::new(
            vec![],
            HashMap::new(),
            configs,
            None,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )],
        );
        let document = Document::new_with_path(
            main.to_string(),
            temp.path().join("main.crn"),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );

        let diagnostics = state
            .diagnostic_engine
            .analyze(&document, Some(&temp.path().join("main.crn")));
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert!(
            messages.contains(&"Unknown resource type: mock.unknown.Widget"),
            "LSP must report the same unknown mock resource type as `carina validate`: {messages:#?}",
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("Required attribute 'name' is missing")),
            "LSP must validate required attributes from the built-in mock catalog: {messages:#?}",
        );
        assert!(
            messages.iter().any(|message| {
                message.contains("multiple 'mock.test.Thing' declarations")
                    && message.contains("Use `let` bindings")
            }),
            "LSP must run attribute-derived identity conflict checks with mock schemas: {messages:#?}",
        );
    }

    #[test]
    fn equal_nested_identity_diagnostic_matches_validate_across_sibling_files() {
        let temp = tempfile::tempdir().unwrap();
        let providers = "provider mock {}\n";
        let budget = r#"mock.budgets.Budget {
  budget = {
    budget_name = "shared-budget"
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#;
        std::fs::write(temp.path().join("providers.crn"), providers).unwrap();
        std::fs::write(temp.path().join("a.crn"), budget).unwrap();
        std::fs::write(temp.path().join("b.crn"), budget).unwrap();

        let discovered = workspace::discover_providers_by_dir(temp.path());
        let configs = discovered[temp.path()]
            .iter()
            .cloned()
            .map(|config| (temp.path().to_path_buf(), config))
            .collect();
        let state = ProviderState::new(
            vec![],
            HashMap::new(),
            configs,
            None,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )],
        );
        let path = temp.path().join("a.crn");
        let document = Document::new_with_path(
            budget.to_string(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );

        let diagnostics = state.diagnostic_engine.analyze(&document, Some(&path));
        let matching = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .message
                    .contains("equal declared identity values")
            })
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
        assert_eq!(
            matching[0].message,
            "Anonymous resources of type 'mock.budgets.Budget' on the same provider route have \
             equal declared identity values and produce address \
             'mock_budgets_budget_d96fb606'. Give one of them a name with `let`."
        );
    }

    const INCOMPLETE_BUDGET_IDENTITY_ERROR: &str = "Anonymous resource of type \
        'mock.budgets.Budget' cannot derive an address because identity value \
        'budget.budget_name' is not available as a written value or authored expression at plan \
        time. Write the identity value or give the resource a name with `let`.";

    fn incomplete_budget_diagnostics(
        sibling_files: &[(&str, &str)],
        budget: &str,
    ) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let temp = tempfile::tempdir().expect("multi-file identity fixture");
        std::fs::write(temp.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            temp.path().join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .unwrap();
        for (name, source) in sibling_files {
            std::fs::write(temp.path().join(name), source).unwrap();
        }
        let path = temp.path().join("budget.crn");
        std::fs::write(&path, budget).unwrap();
        let state = builtin_mock_provider_state(temp.path());
        let document = Document::new_with_path(
            budget.to_string(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );
        state.diagnostic_engine.analyze(&document, Some(&path))
    }

    #[test]
    fn omitted_identity_value_lsp_error_matches_validate_and_plan_in_multi_file_directory() {
        let budget = r#"mock.budgets.Budget {
  budget = {
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#;

        let diagnostics = incomplete_budget_diagnostics(&[], budget);
        let matching = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message == INCOMPLETE_BUDGET_IDENTITY_ERROR)
            .collect::<Vec<_>>();

        assert_eq!(matching.len(), 1, "diagnostics: {diagnostics:#?}");
        assert_eq!(
            matching[0].severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
        );
        assert_eq!(matching[0].range.start.line, 0);
        assert_eq!(matching[0].range.start.character, 0);
    }

    #[test]
    fn apply_time_reference_has_no_incomplete_identity_lsp_error() {
        let source = r#"let source = mock.test.resource {
  name = "identity-source"
}
"#;
        let budget = r#"mock.budgets.Budget {
  budget = {
    budget_name = source.identifier
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#;

        let diagnostics = incomplete_budget_diagnostics(&[("source.crn", source)], budget);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != INCOMPLETE_BUDGET_IDENTITY_ERROR),
            "an authored resource reference is a complete identity value: {diagnostics:#?}",
        );
    }

    #[test]
    fn route53_reference_identity_directory_has_no_lsp_errors() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../carina-cli/tests/fixtures/anonymous_reference_identity");
        let path = fixture.join("records.crn");
        let records = std::fs::read_to_string(&path).expect("read shared RecordSet fixture");
        let state = builtin_mock_provider_state(&fixture);
        let document = Document::new_with_path(
            records,
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );

        let diagnostics = state.diagnostic_engine.analyze(&document, Some(&path));

        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.severity != Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            }),
            "the shared multi-file reference-identity fixture must be LSP-valid: {diagnostics:#?}",
        );
    }

    #[test]
    fn plan_time_module_argument_has_no_incomplete_identity_lsp_error() {
        let temp = tempfile::tempdir().expect("multi-file module identity fixture");
        let module = temp.path().join("budget_module");
        std::fs::create_dir(&module).unwrap();
        std::fs::write(temp.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            temp.path().join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .unwrap();
        std::fs::write(
            module.join("arguments.crn"),
            "arguments {\n  budget_name: String\n}\n",
        )
        .unwrap();
        std::fs::write(
            module.join("budget.crn"),
            r#"mock.budgets.Budget {
  budget = {
    budget_name = budget_name
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#,
        )
        .unwrap();
        let modules = r#"let budget_module = use { source = "./budget_module" }

budget_module {
  budget_name = "module-budget"
}
"#;
        let path = temp.path().join("modules.crn");
        std::fs::write(&path, modules).unwrap();
        let state = builtin_mock_provider_state(temp.path());
        let document = Document::new_with_path(
            modules.to_string(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );

        let diagnostics = state.diagnostic_engine.analyze(&document, Some(&path));

        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.message != INCOMPLETE_BUDGET_IDENTITY_ERROR),
            "a concrete module argument is known at plan time: {diagnostics:#?}",
        );
    }

    #[test]
    fn ignore_changes_validation_matches_core_in_directory_scoped_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let providers = "provider mock {}\n";
        let backend = "backend local { path = \"carina.state.json\" }\n";
        let main = r#"let missing = mock.test.resource {
  name = "missing"
  directives { ignore_changes = [does_not_exist] }
}

let read_only = mock.test.resource {
  name = "read-only"
  directives { ignore_changes = [identifier] }
}

let indexed = mock.test.resource {
  name = "indexed"
  rule { action = "allow" }
  directives { ignore_changes = [rule[0].action] }
}

let union_path = mock.test.resource {
  name = "union"
  choice = { value = "selected" }
  directives { ignore_changes = [choice.value] }
}

let unwritten = mock.test.resource {
  name = "unwritten"
  directives { ignore_changes = [comment] }
}

let duplicate = mock.test.resource {
  name = "duplicate"
  comment = "recorded"
  directives { ignore_changes = [comment, comment] }
}
"#;
        std::fs::write(temp.path().join("providers.crn"), providers).unwrap();
        std::fs::write(temp.path().join("backend.crn"), backend).unwrap();
        std::fs::write(temp.path().join("main.crn"), main).unwrap();

        let discovered = workspace::discover_providers_by_dir(temp.path());
        let configs = discovered[temp.path()]
            .iter()
            .cloned()
            .map(|config| (temp.path().to_path_buf(), config))
            .collect();
        let state = ProviderState::new(
            vec![],
            HashMap::new(),
            configs,
            None,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )],
        );
        let main_path = temp.path().join("main.crn");
        let document = Document::new_with_path(
            main.to_string(),
            main_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );
        let messages = state
            .diagnostic_engine
            .analyze(&document, Some(&main_path))
            .into_iter()
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();

        for expected in [
            "ignore_changes path `does_not_exist` does not exist in the resource schema",
            "ignore_changes path `identifier` reaches a read-only or provider-populated node",
            "ignore_changes path `rule[0].action` contains a list index, which is not supported",
            "ignore_changes path `choice.value` passes through a union",
            "ignore_changes path `comment` is not written in this resource block",
            "ignore_changes path `comment` is listed more than once",
        ] {
            assert!(
                messages.iter().any(|message| message.ends_with(expected)),
                "missing `{expected}` from {messages:#?}",
            );
        }
    }

    #[test]
    fn sourced_mock_load_failure_does_not_activate_builtin_catalog_across_sibling_files() {
        let temp = tempfile::tempdir().unwrap();
        let providers = r#"provider mock {
  source = "file:///definitely-missing/mock.wasm"
}
"#;
        let main = "mock.test.resource {}\n";
        std::fs::write(temp.path().join("providers.crn"), providers).unwrap();
        std::fs::write(temp.path().join("main.crn"), main).unwrap();

        let discovered = workspace::discover_providers_by_dir(temp.path());
        let configs = discovered[temp.path()]
            .iter()
            .cloned()
            .map(|config| (temp.path().to_path_buf(), config))
            .collect();
        let state = ProviderState::new(
            vec![],
            HashMap::from([(
                "mock".to_string(),
                "provider artifact is missing".to_string(),
            )]),
            configs,
            None,
            vec![(
                "mock".to_string(),
                Ok(ProviderArtifactFingerprint::default()),
            )],
        );
        let document = Document::new_with_path(
            main.to_string(),
            temp.path().join("main.crn"),
            Arc::new(carina_core::parser::ProviderContext::default()),
        );

        let diagnostics = state
            .diagnostic_engine
            .analyze(&document, Some(&temp.path().join("main.crn")));
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();

        assert!(
            !state.schemas.has_managed("mock", "test.resource"),
            "a sourced mock whose plugin failed to load must not silently receive the built-in catalog",
        );
        assert!(
            messages.iter().any(|message| {
                message.contains("Provider 'mock' is not loaded")
                    && message.contains("provider artifact is missing")
            }),
            "LSP must report the provider load failure, matching CLI's failure outcome: {messages:#?}",
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("Required attribute 'name' is missing")),
            "the built-in schema must not validate a sourced provider that failed to load: {messages:#?}",
        );
    }

    #[test]
    fn imported_module_uses_lexicographically_first_caller_state() {
        let tmp = tempfile::tempdir().unwrap();
        let first_caller = tmp.path().join("a-caller");
        let last_caller = tmp.path().join("z-caller");
        let module = tmp.path().join("module");
        std::fs::create_dir_all(&first_caller).unwrap();
        std::fs::create_dir_all(&last_caller).unwrap();
        std::fs::create_dir_all(&module).unwrap();

        let mut states = ProviderStates::new();
        states.by_dir.insert(
            PathIdentity::new(&first_caller),
            provider_state("first_rule"),
        );
        states
            .by_dir
            .insert(PathIdentity::new(&last_caller), provider_state("last_rule"));
        states.import_map.insert(
            PathIdentity::new(&module),
            std::collections::BTreeSet::from([
                PathIdentity::new(&last_caller),
                PathIdentity::new(&first_caller),
            ]),
        );

        let state = states.state_for_path(&module);
        let block_names = collect_all_block_names(&state.schemas);

        assert_eq!(
            block_names.get("rules").map(String::as_str),
            Some("first_rule")
        );
    }

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
            ("main.crn", "awscc.s3.Bucket { bucket_name = 'ex' }\n"),
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

    fn test_file_fingerprint(
        path: &Path,
    ) -> Option<carina_provider_resolver::FileMetadataFingerprint> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path).ok()?;
        Some(carina_provider_resolver::FileMetadataFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
        })
    }

    /// Stub prober for tests: records the same cheap metadata shape as the
    /// production resolver without smuggling resolver inputs through provider
    /// configuration attributes.
    fn test_prober(installed_at: Option<PathBuf>) -> ProviderInstallProber {
        Arc::new(move |dir, _cfg| {
            let artifact = installed_at.as_deref().and_then(test_file_fingerprint);
            Ok(ProviderArtifactFingerprint {
                artifact,
                lock_file: test_file_fingerprint(&dir.join("carina-providers.lock")),
            })
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
        use carina_core::resource::ParserPlaceholder;

        let tmp = tempfile::tempdir().unwrap();
        let installed = tmp.path().join("carina-provider-foo.wasm");
        std::fs::write(&installed, b"fake").unwrap();

        let config =
            workspace::DiscoveredProviderConfig::from_parser(ProviderConfig::<ParserPlaceholder> {
                name: "foo".into(),
                source: Some("github.com/carina-rs/stub".parse().unwrap()),
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
            });
        let configs = vec![(tmp.path().to_path_buf(), config)];
        let prober = test_prober(Some(installed.clone()));

        let before = probe_install_fingerprint(&prober, &configs);
        assert!(
            before[0].1.as_ref().unwrap().artifact.is_some(),
            "initial probe should see the installed binary metadata"
        );

        std::fs::remove_file(&installed).unwrap();
        let after = probe_install_fingerprint(&prober, &configs);
        assert!(
            after[0].1.as_ref().unwrap().artifact.is_none(),
            "probe after delete must record missing artifact metadata"
        );
        assert_ne!(before, after);
    }

    #[test]
    fn install_fingerprint_stable_when_nothing_changes() {
        use carina_core::parser::ProviderConfig;
        use carina_core::resource::ParserPlaceholder;
        let tmp = tempfile::tempdir().unwrap();
        let config =
            workspace::DiscoveredProviderConfig::from_parser(ProviderConfig::<ParserPlaceholder> {
                name: "missing".into(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
            });
        let configs = vec![(tmp.path().to_path_buf(), config)];
        let prober = test_prober(None);
        assert_eq!(
            probe_install_fingerprint(&prober, &configs),
            probe_install_fingerprint(&prober, &configs),
            "two back-to-back probes with no fs change must agree"
        );
    }
}

#[cfg(test)]
mod reload_skip_tests {
    use super::*;
    use carina_core::parser::ProviderConfig;
    use carina_core::resource::{ConcreteValue, ParserPlaceholder, Value};
    use indexmap::IndexMap;

    fn mk_config(name: &str, source: Option<&str>) -> ProviderConfig<ParserPlaceholder> {
        ProviderConfig {
            name: name.into(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: source.map(|source| source.parse().unwrap()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        }
    }

    fn discovered(
        config: ProviderConfig<ParserPlaceholder>,
    ) -> workspace::DiscoveredProviderConfig {
        workspace::DiscoveredProviderConfig::from_parser(config)
    }

    // #2136: save of a non-provider `.crn` must not trigger a
    // FactoryBuilder rebuild. `configs_match` is the gate that keeps
    // `load_schemas_impl` from re-running the expensive WASM parse when
    // the on-disk provider config didn't actually change.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_provider_directories_reuse_factories_on_reload() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("providers.crn"), "provider mock {}\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let builds = Arc::new(AtomicUsize::new(0));
        let builder: FactoryBuilder = Arc::new({
            let builds = Arc::clone(&builds);
            move |configs| {
                assert_eq!(configs.len(), 1);
                builds.fetch_add(1, Ordering::SeqCst);
                (vec![], HashMap::new(), vec![])
            }
        });
        let (service, socket) = tower_lsp::LspService::new(|client| {
            Backend::new(client, ProviderContext::default(), Some(builder))
        });
        drop(socket);
        let backend = service.inner();
        backend
            .workspace_root
            .set(Some(temp.path().to_path_buf()))
            .unwrap();
        backend.load_schemas().await;
        assert_eq!(builds.load(Ordering::SeqCst), 1, "aliases must load once");
        let schemas = Arc::clone(&backend.providers.read().await.state_for_path(&real).schemas);
        backend.load_schemas().await;
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "reload must reuse the factory"
        );
        assert!(Arc::ptr_eq(
            &schemas,
            &backend.providers.read().await.state_for_path(&link).schemas,
        ));
        // A new preferred alias changes display spelling, not configuration.
        std::os::unix::fs::symlink(&real, temp.path().join("alias")).unwrap();
        backend.load_schemas().await;
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn configs_match_returns_true_for_identical_lists() {
        let dir = std::path::PathBuf::from("/tmp/x");
        let a = vec![(
            dir.clone(),
            discovered(mk_config(
                "awscc",
                Some("github.com/carina-rs/carina-provider-awscc"),
            )),
        )];
        let b = vec![(
            dir,
            discovered(mk_config(
                "awscc",
                Some("github.com/carina-rs/carina-provider-awscc"),
            )),
        )];
        assert!(configs_match(&a, &b));
    }

    #[test]
    fn configs_match_returns_false_when_source_changes() {
        let dir = std::path::PathBuf::from("/tmp/x");
        let a = vec![(
            dir.clone(),
            discovered(mk_config(
                "awscc",
                Some("github.com/carina-rs/carina-provider-awscc"),
            )),
        )];
        let b = vec![(
            dir,
            discovered(mk_config("awscc", Some("file:///local/awscc"))),
        )];
        assert!(!configs_match(&a, &b));
    }

    #[test]
    fn configs_match_returns_false_when_provider_added() {
        let dir = std::path::PathBuf::from("/tmp/x");
        let a = vec![(
            dir.clone(),
            discovered(mk_config(
                "awscc",
                Some("github.com/carina-rs/carina-provider-awscc"),
            )),
        )];
        let b = vec![
            (
                dir.clone(),
                discovered(mk_config(
                    "awscc",
                    Some("github.com/carina-rs/carina-provider-awscc"),
                )),
            ),
            (
                dir,
                discovered(mk_config(
                    "aws",
                    Some("github.com/carina-rs/carina-provider-aws"),
                )),
            ),
        ];
        assert!(!configs_match(&a, &b));
    }

    #[test]
    fn configs_match_returns_false_when_attributes_change() {
        let dir = std::path::PathBuf::from("/tmp/x");
        let mut cfg_a = mk_config("awscc", Some("github.com/carina-rs/carina-provider-awscc"));
        cfg_a.attributes.insert(
            "region".into(),
            Value::Concrete(ConcreteValue::String("us-east-1".into())),
        );
        let mut cfg_b = mk_config("awscc", Some("github.com/carina-rs/carina-provider-awscc"));
        cfg_b.attributes.insert(
            "region".into(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        );
        let a = vec![(dir.clone(), discovered(cfg_a))];
        let b = vec![(dir, discovered(cfg_b))];
        assert!(!configs_match(&a, &b));
    }

    #[test]
    fn discovered_configs_match_detects_region_changes() {
        let fixture = tempfile::tempdir().unwrap();
        let provider_file = fixture.path().join("provider.crn");
        std::fs::write(&provider_file, "provider awscc { region = 'us-east-1' }\n").unwrap();
        let first = workspace::discover_providers_by_dir(fixture.path())
            .remove(fixture.path())
            .unwrap()
            .into_iter()
            .map(|config| (fixture.path().to_path_buf(), config))
            .collect::<Vec<_>>();

        std::fs::write(
            &provider_file,
            "provider awscc { region = 'ap-northeast-1' }\n",
        )
        .unwrap();
        let second = workspace::discover_providers_by_dir(fixture.path())
            .remove(fixture.path())
            .unwrap()
            .into_iter()
            .map(|config| (fixture.path().to_path_buf(), config))
            .collect::<Vec<_>>();

        assert!(!configs_match(&first, &second));
    }

    #[test]
    fn discovered_provider_factory_projection_is_attribute_free() {
        let fixture = tempfile::tempdir().unwrap();
        let provider_file = fixture.path().join("provider.crn");
        let discover = || {
            workspace::discover_providers_by_dir(fixture.path())
                .remove(fixture.path())
                .unwrap()
                .into_iter()
                .map(|config| (fixture.path().to_path_buf(), config))
                .collect::<Vec<_>>()
        };

        std::fs::write(&provider_file, "provider awscc {}\n").unwrap();
        let missing = discover();

        std::fs::write(
            &provider_file,
            "provider awscc { region = 'ap-northeast-1' profile = \"${}\" }\n",
        )
        .unwrap();
        let placeholder = discover();
        assert!(
            placeholder[0]
                .1
                .comparison_key()
                .attributes
                .contains_key("profile"),
            "the discovery key must retain the parser-stage placeholder",
        );
        assert!(
            placeholder[0]
                .1
                .comparison_key()
                .attributes
                .contains_key("region"),
            "the discovery key must retain concrete attributes too",
        );
        assert!(
            placeholder[0].1.factory_config().attributes.is_empty(),
            "provider loading reads only identity/source metadata and must receive no attributes",
        );

        std::fs::write(
            &provider_file,
            "provider awscc { region = 'ap-northeast-1' }\n",
        )
        .unwrap();
        let concrete = discover();

        assert!(
            !configs_match(&missing, &placeholder),
            "a mid-edit placeholder must not collapse to a missing attribute",
        );
        assert!(
            !configs_match(&placeholder, &concrete),
            "a placeholder and concrete region must produce different discovery keys",
        );
    }

    #[test]
    fn configs_match_true_regardless_of_attribute_insertion_order() {
        // HashMap iteration order is unspecified; configs_match must not
        // depend on the order in which attributes were inserted.
        let dir = std::path::PathBuf::from("/tmp/x");
        let mut cfg_a = mk_config("awscc", Some("github.com/carina-rs/carina-provider-awscc"));
        cfg_a.attributes.insert(
            "region".into(),
            Value::Concrete(ConcreteValue::String("us-east-1".into())),
        );
        cfg_a.attributes.insert(
            "profile".into(),
            Value::Concrete(ConcreteValue::String("dev".into())),
        );
        let mut cfg_b = mk_config("awscc", Some("github.com/carina-rs/carina-provider-awscc"));
        cfg_b.attributes.insert(
            "profile".into(),
            Value::Concrete(ConcreteValue::String("dev".into())),
        );
        cfg_b.attributes.insert(
            "region".into(),
            Value::Concrete(ConcreteValue::String("us-east-1".into())),
        );
        let a = vec![(dir.clone(), discovered(cfg_a))];
        let b = vec![(dir, discovered(cfg_b))];
        assert!(configs_match(&a, &b));
    }
}
