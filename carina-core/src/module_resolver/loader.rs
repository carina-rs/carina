//! Filesystem loading helpers for modules.
//!
//! Modules are directory-scoped: every helper here treats a module as a
//! directory containing one or more `.crn` files, never as a single file.

use crate::paths::ResolvedPath;
#[cfg(test)]
use std::fs;

use crate::parser::{ParsedFile, ProviderContext};

use super::error::ModuleError;

/// A parsed module plus non-blocking diagnostics discovered while its sibling
/// files were still separate.
pub struct LoadedModule {
    directory: ResolvedPath,
    parsed: ParsedFile,
    pub duplicate_declarations: Vec<crate::config_loader::DuplicateDeclaration>,
}

/// Declaration source for hover, completion and call signatures. This is not
/// evidence that the directory may be imported or its body expanded.
#[derive(Debug)]
pub struct ModuleSignatureSource {
    pub arguments: Vec<crate::parser::ArgumentParameter>,
    pub attribute_params: Vec<crate::parser::AttributeParameter>,
    // Output names and declared types preserve main's reference completion.
    // Export values, body bindings and the syntax inventory are never kept.
    export_types: Vec<(String, crate::parser::TypeExpr)>,
    requires: Vec<
        crate::parser::RequireBlock<
            crate::resource::ParserPlaceholder,
            crate::resource::RelativeBindingName,
        >,
    >,
}

impl ModuleSignatureSource {
    fn from_parsed(parsed: ParsedFile) -> Self {
        Self {
            arguments: parsed.arguments,
            attribute_params: parsed.attribute_params,
            export_types: parsed
                .export_params
                .into_iter()
                .filter_map(|export| {
                    export
                        .static_type
                        .map(|static_type| (export.name, static_type))
                })
                .collect(),
            requires: parsed.requires,
        }
    }

    pub fn export_types(&self) -> impl Iterator<Item = (&str, &crate::parser::TypeExpr)> {
        self.export_types
            .iter()
            .map(|(name, static_type)| (name.as_str(), static_type))
    }

    pub(super) fn resolve<P: super::resolver::TypeResolutionPolicy>(
        &self,
        config: &ProviderContext,
    ) -> super::ResolvedModuleSignature {
        super::signature::resolved_signature_declarations_with_policy::<P>(
            &self.arguments,
            &self.attribute_params,
            &self.requires,
            config,
        )
    }
}

/// A parsed empty interface is distinct from a source that could not be
/// parsed with the caller's provider context. Neither grants body authority.
#[derive(Debug)]
pub enum ModuleSignatureView {
    Declarations(Box<ModuleSignatureSource>),
    EmptyInterface,
    Unavailable,
}

/// A preflight copy can originate only from a loader-produced module.
/// Its mutable operations resolve declarations without replacing its syntax.
pub struct ModuleTemplate {
    parsed: ParsedFile,
}

/// Judgement evidence for the separate module-template preflight boundary.
/// Root configuration judgements cannot enter this boundary.
pub struct ModuleTemplatePreflight<'a> {
    configuration: crate::identifier::PrefixCanonicalized<
        'a,
        crate::parser::retained::JudgedFile<
            'a,
            crate::parser::ParsedExportParam,
            crate::parser::ParsedDeferredForPhase<
                crate::resource::ParserPlaceholder,
                crate::resource::RelativeBindingName,
            >,
        >,
    >,
}

impl LoadedModule {
    /// Directory pinned by the loader before parsing its siblings.
    pub fn directory(&self) -> &ResolvedPath {
        &self.directory
    }

    pub fn parsed(&self) -> &ParsedFile {
        &self.parsed
    }

    pub(super) fn into_parsed(self) -> ParsedFile {
        self.parsed
    }

    pub fn preflight(&self) -> ModuleTemplate {
        ModuleTemplate {
            parsed: self.parsed.clone(),
        }
    }
}

impl ModuleTemplate {
    pub fn parsed(&self) -> &ParsedFile {
        &self.parsed
    }

    pub fn resolve_type_exprs(
        &mut self,
        context: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<
        Vec<crate::parser::Located<crate::validation::TypeResolutionDiagnostic>>,
        crate::parser::AnalysisTreeError,
    > {
        crate::validation::resolve_file_type_exprs(&mut self.parsed, context)
    }

    pub fn judge<'a>(
        &self,
        schemas: &'a crate::schema::SchemaRegistry,
        context: &ProviderContext,
    ) -> ModuleTemplatePreflight<'a> {
        ModuleTemplatePreflight {
            configuration: crate::identifier::canonicalize_file_prefixes(
                self.parsed.clone(),
                schemas,
                crate::identifier::SuffixSource::Preview,
            )
            .judge(context),
        }
    }
}

impl<'a> ModuleTemplatePreflight<'a> {
    pub fn judged(
        &self,
    ) -> &crate::identifier::PrefixCanonicalized<
        'a,
        crate::parser::retained::JudgedFile<
            'a,
            crate::parser::ParsedExportParam,
            crate::parser::ParsedDeferredForPhase<
                crate::resource::ParserPlaceholder,
                crate::resource::RelativeBindingName,
            >,
        >,
    > {
        &self.configuration
    }
}

/// Get parsed file info for display (supports both module definitions and root configs)
pub fn get_parsed_file(
    path: &ResolvedPath,
    config: &ProviderContext,
) -> Result<ParsedFile, ModuleError> {
    let path = path.clone();
    let content = path.read_to_string()?;
    let parsed = crate::parser::parse_file(&content, &path.source_label(), config)?;
    Ok(parsed)
}

/// A failed module load distinguishes unavailable source from a directory that
/// was read successfully but cannot be imported. Static consumers may skip
/// unavailable sources; they must not interpret that as proof of importability.
#[derive(Debug, thiserror::Error)]
pub enum ModuleLoadError {
    #[error("{0}")]
    Unavailable(ModuleError),
    #[error("{error}")]
    NotModule {
        error: Box<ModuleRejection>,
        /// Imports remain available to the static walk without exposing the
        /// rejected directory's expressions or placement report.
        imports: Vec<crate::parser::UseStatement>,
        /// A declaration-only consumer may recover this view explicitly. It
        /// cannot be admitted as a successfully loaded module.
        signature: ModuleSignatureView,
    },
}

/// Successfully inspected inputs that cannot be imported. The payload cannot
/// contain an unrelated resolver error or grow a silently ignored cause.
#[derive(Debug, Clone)]
pub enum ModuleRejection {
    NotADirectory { path: String },
    NoConfigurationFiles { path: crate::paths::DisplayPath },
    Prohibited(ModuleProhibitions),
}

/// One category in an imported directory's complete prohibition set.
#[derive(Debug, Clone)]
pub enum ModuleProhibition {
    Provider {
        evidence: crate::config_loader::RootEvidence,
    },
    Backend {
        evidence: crate::config_loader::RootEvidence,
    },
    State,
    UpstreamState,
    Exports,
}

/// The presentation order used by main's static validation consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModuleProhibitionCategory {
    Provider,
    State,
    Backend,
    UpstreamState,
    Exports,
}

impl ModuleProhibitionCategory {
    pub const ALL: [Self; 5] = [
        Self::Provider,
        Self::State,
        Self::Backend,
        Self::UpstreamState,
        Self::Exports,
    ];
}

impl ModuleProhibition {
    fn category(&self) -> ModuleProhibitionCategory {
        match self {
            Self::Provider { evidence: _ } => ModuleProhibitionCategory::Provider,
            Self::State => ModuleProhibitionCategory::State,
            Self::Backend { evidence: _ } => ModuleProhibitionCategory::Backend,
            Self::UpstreamState => ModuleProhibitionCategory::UpstreamState,
            Self::Exports => ModuleProhibitionCategory::Exports,
        }
    }

    pub fn into_error(self) -> ModuleError {
        match self {
            Self::Provider { evidence } => ModuleError::ProviderInModule { evidence },
            Self::Backend { evidence } => ModuleError::BackendInModule { evidence },
            Self::State => ModuleError::StateBlockInModule,
            Self::UpstreamState => ModuleError::UpstreamStateInModule,
            Self::Exports => ModuleError::ExportsInModule,
        }
    }
}

/// A nonempty, complete set issued only by the imported-directory loader.
/// Private fields prevent a consumer from replacing it with a selected leaf.
#[derive(Debug, Clone)]
pub struct ModuleProhibitions {
    first: ModuleProhibition,
    rest: Vec<ModuleProhibition>,
}

impl ModuleProhibitions {
    /// Collect authored block presence once, before any body resolution. Root
    /// evidence supplies a guaranteed first prohibition; empty blocks count.
    fn collect(
        parsed: &ParsedFile,
        verdict: &crate::config_loader::CompleteDirectoryVerdict,
    ) -> Option<Self> {
        use crate::parser::ConfigurationBlock;
        let mut body = [
            (ConfigurationBlock::State, ModuleProhibition::State),
            (
                ConfigurationBlock::UpstreamState,
                ModuleProhibition::UpstreamState,
            ),
            (ConfigurationBlock::Exports, ModuleProhibition::Exports),
        ]
        .into_iter()
        .filter_map(|(kind, prohibition)| {
            parsed
                .parser_binding_metadata
                .syntax()
                .configuration_blocks()
                .iter()
                .any(|block| block.detail().kind() == kind)
                .then_some(prohibition)
        });
        let (first, mut rest) = match verdict {
            crate::config_loader::DirectoryVerdict::Root(root) => root.import_prohibitions(),
            crate::config_loader::DirectoryVerdict::ModuleTemplate(_)
            | crate::config_loader::DirectoryVerdict::NoEvidence(_) => (body.next()?, Vec::new()),
            crate::config_loader::DirectoryVerdict::Unknown(unknown) => match *unknown {},
        };
        rest.extend(body);
        // Keep main's precedence within a directory too (notably State
        // before Backend). Sort only the producer's closed typed categories.
        let mut members = std::iter::once(first).chain(rest).collect::<Vec<_>>();
        members.sort_by_key(ModuleProhibition::category);
        let first = members.remove(0); // The original first proves nonempty.
        Some(Self {
            first,
            rest: members,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModuleProhibition> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    pub(super) fn into_parts(self) -> (ModuleProhibition, Vec<ModuleProhibition>) {
        (self.first, self.rest)
    }

    pub fn into_errors(self) -> Vec<ModuleError> {
        std::iter::once(self.first)
            .chain(self.rest)
            .map(ModuleProhibition::into_error)
            .collect()
    }

    /// Publish the loader's typed members with ownership, retaining their
    /// categories so consumers can preserve main's category-first ordering.
    pub fn into_owned_errors(
        self,
        location: crate::parser::DeclarationLocation,
        calls: Vec<crate::parser::ModuleCallId>,
    ) -> Vec<(ModuleProhibitionCategory, ModuleError)> {
        std::iter::once(self.first)
            .chain(self.rest)
            .map(|prohibition| {
                let category = prohibition.category();
                let error = prohibition.into_error();
                let error = if calls.is_empty() {
                    ModuleError::AtImport {
                        location: location.clone(),
                        error: Box::new(error),
                    }
                } else {
                    error.with_owning_calls(calls.clone())
                };
                (category, error)
            })
            .collect()
    }

    pub(crate) fn into_error(self) -> ModuleError {
        ModuleError::from_import_prohibitions(self)
    }
}
impl ModuleRejection {
    pub fn into_errors(self) -> Vec<ModuleError> {
        match self {
            Self::NotADirectory { path } => vec![ModuleError::NotADirectory { path }],
            Self::NoConfigurationFiles { path } => {
                vec![ModuleError::NoConfigurationFiles { path }]
            }
            Self::Prohibited(prohibitions) => prohibitions.into_errors(),
        }
    }
    pub(crate) fn into_error(self) -> ModuleError {
        match self {
            Self::NotADirectory { path } => ModuleError::NotADirectory { path },
            Self::NoConfigurationFiles { path } => ModuleError::NoConfigurationFiles { path },
            Self::Prohibited(prohibitions) => prohibitions.into_error(),
        }
    }
}
impl std::fmt::Display for ModuleRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.clone().into_error().fmt(formatter)
    }
}
impl std::error::Error for ModuleRejection {}
impl ModuleLoadError {
    pub(crate) fn into_error(self) -> ModuleError {
        match self {
            Self::Unavailable(error) => error,
            Self::NotModule {
                error,
                imports: _,
                signature: _,
            } => error.into_error(),
        }
    }
    pub fn into_errors(self) -> Vec<ModuleError> {
        match self {
            Self::Unavailable(ModuleError::Multiple(errors)) => errors.into_vec(),
            Self::Unavailable(error) => vec![error],
            Self::NotModule {
                error,
                imports: _,
                signature: _,
            } => error.into_errors(),
        }
    }
    fn rejected(error: ModuleRejection) -> Self {
        Self::NotModule {
            error: Box::new(error),
            imports: Vec::new(),
            signature: ModuleSignatureView::Unavailable,
        }
    }
}

/// Load an imported directory. Empty entry lists do not affect importability.
pub fn load_module(
    path: &ResolvedPath,
    config: &ProviderContext,
) -> Result<ParsedFile, ModuleLoadError> {
    load_module_with_diagnostics(path, config).map(|loaded| loaded.parsed)
}

pub fn load_module_with_diagnostics(
    path: &ResolvedPath,
    config: &ProviderContext,
) -> Result<LoadedModule, ModuleLoadError> {
    load_module_with_diagnostics_and_schemas(
        path,
        config,
        &crate::schema::SchemaRegistry::new(),
        &path.source_label(),
    )
}

pub fn load_module_with_diagnostics_and_schemas(
    path: &ResolvedPath,
    config: &ProviderContext,
    schemas: &crate::schema::SchemaRegistry,
    display_root: &std::path::Path,
) -> Result<LoadedModule, ModuleLoadError> {
    load_imported_directory(path, config, &Default::default(), schemas, display_root)
}

/// Read and admit a directory through the same import boundary as the resolver.
pub fn load_directory_module(
    path: &ResolvedPath,
    config: &ProviderContext,
) -> Result<ParsedFile, ModuleLoadError> {
    load_module(path, config)
}

pub(crate) fn load_module_with_overrides(
    path: &ResolvedPath,
    config: &ProviderContext,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<ParsedFile, ModuleLoadError> {
    load_imported_directory(
        path,
        config,
        overrides,
        &crate::schema::SchemaRegistry::new(),
        &path.source_label(),
    )
    .map(|module| module.parsed)
}

pub(crate) fn load_signature_source_with_overrides(
    path: &ResolvedPath,
    config: &ProviderContext,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<ModuleSignatureSource, ModuleLoadError> {
    load_signature_source(
        path,
        config,
        overrides,
        &crate::schema::SchemaRegistry::new(),
        &path.source_label(),
    )
}

/// Inspect declarations without granting import or body-analysis authority.
pub fn load_module_signature_with_schemas(
    path: &ResolvedPath,
    config: &ProviderContext,
    schemas: &crate::schema::SchemaRegistry,
    display_root: &std::path::Path,
) -> Result<ModuleSignatureSource, ModuleLoadError> {
    load_signature_source(path, config, &Default::default(), schemas, display_root)
}

fn load_signature_source(
    path: &ResolvedPath,
    config: &ProviderContext,
    overrides: &crate::config_loader::SourceOverrides,
    schemas: &crate::schema::SchemaRegistry,
    display_root: &std::path::Path,
) -> Result<ModuleSignatureSource, ModuleLoadError> {
    match signature_source(load_imported_directory(
        path,
        config,
        overrides,
        schemas,
        display_root,
    )) {
        Ok(source) => Ok(source),
        // Main's static loader skips unreadable siblings. Recover only an
        // interface from readable files, using the same provider context.
        Err(ModuleLoadError::Unavailable(error @ ModuleError::Io(_))) => {
            readable_signature(path, config, overrides).ok_or(ModuleLoadError::Unavailable(error))
        }
        Err(ModuleLoadError::Unavailable(error)) => Err(ModuleLoadError::Unavailable(error)),
        Err(ModuleLoadError::NotModule {
            error,
            imports,
            signature,
        }) => Err(ModuleLoadError::NotModule {
            error,
            imports,
            signature,
        }),
    }
}

fn readable_signature(
    path: &ResolvedPath,
    config: &ProviderContext,
    overrides: &crate::config_loader::SourceOverrides,
) -> Option<ModuleSignatureSource> {
    let paths = crate::config_loader::DirectoryFiles::discover(path, overrides)
        .ok()?
        .into_paths();
    let files = paths
        .into_iter()
        .filter_map(|path| {
            crate::config_loader::read_source_with_overrides(&path, overrides)
                .ok()
                .map(|source| (path.source_label(), source))
        })
        .collect::<Vec<_>>();
    let parsed_files = crate::config_loader::parse_directory_files(&files, config).ok()?;
    let mut interface = ModuleSignatureSource {
        arguments: Vec::new(),
        attribute_params: Vec::new(),
        export_types: Vec::new(),
        requires: Vec::new(),
    };
    for (_, parsed) in parsed_files {
        let parsed = parsed.into_inner();
        interface.arguments.extend(parsed.arguments);
        interface.attribute_params.extend(parsed.attribute_params);
        interface
            .export_types
            .extend(parsed.export_params.into_iter().filter_map(|export| {
                export
                    .static_type
                    .map(|static_type| (export.name, static_type))
            }));
        interface.requires.extend(parsed.requires);
    }
    (!interface.arguments.is_empty() || !interface.attribute_params.is_empty()).then_some(interface)
}

fn signature_source(
    result: Result<LoadedModule, ModuleLoadError>,
) -> Result<ModuleSignatureSource, ModuleLoadError> {
    match result {
        Ok(module) => Ok(ModuleSignatureSource::from_parsed(module.parsed)),
        Err(ModuleLoadError::NotModule {
            error: _,
            imports: _,
            signature: ModuleSignatureView::Declarations(source),
        }) => Ok(*source),
        Err(ModuleLoadError::NotModule {
            error,
            imports,
            signature:
                signature @ (ModuleSignatureView::EmptyInterface | ModuleSignatureView::Unavailable),
        }) => Err(ModuleLoadError::NotModule {
            error,
            imports,
            signature,
        }),
        Err(ModuleLoadError::Unavailable(error)) => Err(ModuleLoadError::Unavailable(error)),
    }
}

/// The shared import boundary consumes the issued verdict before exposing a
/// parsed module to signature, expression or expansion consumers. A rejected
/// root can therefore publish its in-module prohibition without also publishing
/// root-placement findings from that directory.
pub(crate) fn load_imported_directory(
    dir_path: &ResolvedPath,
    config: &ProviderContext,
    overrides: &crate::config_loader::SourceOverrides,
    schemas: &crate::schema::SchemaRegistry,
    display_root: &std::path::Path,
) -> Result<LoadedModule, ModuleLoadError> {
    use ModuleLoadError::Unavailable;
    let metadata = dir_path
        .metadata()
        .map_err(|e| Unavailable(ModuleError::Io(e)))?;
    if !metadata.is_dir() {
        return Err(ModuleLoadError::rejected(ModuleRejection::NotADirectory {
            path: dir_path.source_label().to_string_lossy().into_owned(),
        }));
    }
    let inputs = crate::config_loader::DirectoryFiles::discover(dir_path, overrides)
        .map_err(|e| Unavailable(ModuleError::Io(e)))?
        .read_all(overrides)
        .map_err(|e| Unavailable(ModuleError::Io(e.into_error())))?;
    let unavailable = |located: crate::config_loader::LocatedParseError| {
        Unavailable(match located.source_file {
            Some(path) => ModuleError::ParseInFile {
                path,
                error: Box::new(*located.error),
            },
            None => ModuleError::Parse(*located.error),
        })
    };
    let (parsed_files, body_error) = match inputs.parse_from_root(config, display_root) {
        Ok(files) => (files, None),
        Err(error) => {
            let neutral = ProviderContext::new(config.environment.clone());
            match inputs.parse_from_root(&neutral, display_root) {
                Ok(files) => (files, Some(unavailable(error))),
                Err(_) => return Err(unavailable(error)),
            }
        }
    };
    if parsed_files.is_empty() {
        let label = dir_path.source_label();
        let display = ResolvedPath::new(display_root)
            .ok()
            .map(|base| crate::paths::DisplayPath::for_diagnostics_from(&label, &base))
            .unwrap_or_else(|| crate::paths::DisplayPath::for_diagnostics(&label));
        let display = if matches!(display.to_string().as_str(), "" | ".") {
            crate::paths::DisplayPath::for_diagnostics(dir_path.persisted_path().to_path_buf())
        } else {
            display
        };
        return Err(ModuleLoadError::rejected(
            ModuleRejection::NoConfigurationFiles { path: display },
        ));
    }
    let duplicate_declarations =
        crate::config_loader::find_duplicate_declarations(&dir_path.source_label(), &parsed_files);
    let mut merged = parsed_files.empty_assembly();
    let (verdict, parsed_files) = parsed_files.into_parts();
    for (_, parsed) in parsed_files {
        crate::config_loader::merge_parsed_file(&mut merged, parsed.into_inner());
    }
    if let Some(prohibitions) = ModuleProhibitions::collect(&merged, &verdict) {
        let imports = merged.uses.clone();
        // Body analysis and expansion never receive prohibited directories.
        // Static UI consumers request their declaration source explicitly.
        // Match main's declaration availability, including argumented roots,
        // without granting any body-analysis or expansion authority.
        let signature = if body_error.is_some() {
            // The neutral parse classifies prohibition only. Main's UI did
            // not expose declarations after an enriched parse failure.
            ModuleSignatureView::Unavailable
        } else if crate::validation::is_module_definition(&merged) {
            ModuleSignatureView::Declarations(Box::new(ModuleSignatureSource::from_parsed(merged)))
        } else {
            ModuleSignatureView::EmptyInterface
        };
        return Err(ModuleLoadError::NotModule {
            error: Box::new(ModuleRejection::Prohibited(prohibitions)),
            imports,
            signature,
        });
    }
    if let Some(error) = body_error {
        return Err(error);
    }
    crate::parser::retained::judge_configuration(&mut merged, schemas, config);
    let module = LoadedModule {
        directory: dir_path.clone(),
        parsed: merged,
        duplicate_declarations,
    };
    Ok(module)
}

/// Derive the module name from a file or directory path.
///
/// Examples:
/// - `modules/web_tier/` → `web_tier` (directory)
/// - `modules/web_tier/main.crn` → `web_tier` (directory-based)
/// - `modules/web_tier.crn` → `web_tier` (file-based)
/// - `web_tier.crn` → `web_tier`
pub fn derive_module_name(path: &ResolvedPath) -> String {
    let label = path.source_label();
    if path.is_dir() {
        return label
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
    }

    let file_stem = label
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // If file is named main.crn, use the parent directory name
    if file_stem == "main"
        && let Some(parent) = label.parent()
        && let Some(parent_name) = parent.file_name()
        && let Some(name) = parent_name.to_str()
    {
        return name.to_string();
    }

    file_stem.to_string()
}

/// Load a module from a directory by reading all `.crn` files.
///
/// This direct-entry metadata API retains the issued verdict and supports
/// roots and templates. Unlike the import APIs, it does not admit the directory
/// for `use` or run in-module prohibitions.
///
/// Routed through [`crate::config_loader::parse_directory_files`] for
/// directory-aware parse (#2817).
pub fn load_module_from_directory(
    dir: &ResolvedPath,
    config: &ProviderContext,
) -> Result<ParsedFile, String> {
    let dir = dir.clone();
    let inputs = crate::config_loader::DirectoryFiles::discover(&dir, &Default::default())
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;
    let inputs = inputs
        .read_all(&Default::default())
        .map_err(|e| e.to_string())?;
    let parsed_files = inputs
        .parse_from_root(config, &dir.source_label())
        .map_err(|e| {
            format!(
                "Failed to parse directory {}: {}",
                dir.display(),
                e.error.as_ref()
            )
        })?;

    let mut merged = parsed_files.empty_assembly();
    for (_, parsed) in parsed_files {
        crate::config_loader::merge_parsed_file(&mut merged, parsed.into_inner());
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn module_metadata_load_retains_root_verdict_and_require_finding() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("backend.crn"),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
        fs::write(dir.path().join("arguments.crn"), "arguments {}\n").unwrap();
        fs::write(
            dir.path().join("guards.crn"),
            "require false, 'PRIVATE_GUARD'\n",
        )
        .unwrap();
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let loaded =
            load_module_from_directory(&ResolvedPath::new(dir.path()).unwrap(), &config).unwrap();
        assert!(matches!(
            loaded
                .directory_verdict()
                .expect("issued directory verdict"),
            crate::config_loader::DirectoryVerdict::Root(_)
        ));
        let schemas = crate::schema::SchemaRegistry::new();
        let judged = crate::config_loader::load_buffer_configuration(
            crate::config_loader::issue_buffer_configuration(&loaded),
            &schemas,
            &config,
        );
        assert_eq!(judged.judged().report().findings().len(), 1);
        assert!(
            judged.judged().report().findings()[0]
                .detail()
                .to_string()
                .starts_with("`require` is only allowed inside a module\nNote: ")
        );
    }

    #[test]
    fn load_module_from_directory_preserves_pest_syntax_diagnostic() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("broken.crn"), "let =\n").unwrap();

        let error = load_module_from_directory(
            &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect_err("invalid module syntax must fail to load");

        assert!(error.contains("Syntax error"), "{error}");
        assert!(error.contains("--> 1:"), "missing pest location: {error}");
        assert_eq!(
            error.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {error}"
        );
        assert!(
            error.contains("expected"),
            "missing pest expectation: {error}"
        );
    }

    /// Loader is directory-scoped. Multi-file fixture asserts that sibling
    /// `.crn` files in the same module directory are merged into the
    /// returned `ParsedFile` — never read in isolation.
    #[test]
    fn load_module_merges_sibling_crn_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("main.crn"), "# main module file\n").unwrap();
        fs::write(dir.join("arguments.crn"), "arguments {\n  env: String\n}\n").unwrap();
        fs::write(
            dir.join("exports.crn"),
            "exports {\n  region = \"ap-northeast-1\"\n}\n",
        )
        .unwrap();

        let parsed = load_module_from_directory(
            &crate::paths::ResolvedPath::new(dir).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("the metadata loader preserves every sibling declaration");
        assert_eq!(parsed.arguments.len(), 1);
        assert_eq!(parsed.arguments[0].name, "env");
        assert_eq!(parsed.export_params.len(), 1);
        assert_eq!(parsed.export_params[0].name, "region");
    }

    #[test]
    fn imported_root_reports_main_category_order_and_keeps_each_evidence() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a-provider.crn"), "provider mock {}\n").unwrap();
        fs::write(
            tmp.path().join("b-backend.crn"),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("state.crn"),
            "removed { from = mock.test.resource 'old' }\n",
        )
        .unwrap();
        let error = load_module_with_diagnostics(
            &ResolvedPath::new(tmp.path()).unwrap(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .err()
        .expect("root is not importable");
        let errors = match &error {
            ModuleLoadError::NotModule { error, .. } => error.clone().into_errors(),
            _ => panic!("root"),
        };
        assert!(matches!(
            errors[0].root_cause(),
            ModuleError::ProviderInModule { evidence: _ }
        ));
        assert!(matches!(
            errors[2].root_cause(),
            ModuleError::BackendInModule { evidence: _ }
        ));
        assert!(matches!(
            errors[1].root_cause(),
            ModuleError::StateBlockInModule
        ));
        assert!(
            errors[0]
                .root_evidence()
                .unwrap()
                .note()
                .contains("a-provider.crn:1.")
        );
        assert!(
            errors[2]
                .root_evidence()
                .unwrap()
                .note()
                .contains("b-backend.crn:1.")
        );
        assert_eq!(
            errors[2].root_evidence().unwrap().related_message(),
            "This backend block makes the directory a root configuration."
        );
        let combined = error.into_error();
        assert!(combined.root_evidence().is_some());
        assert_eq!(
            combined.lsp_message_relative_to(tmp.path()),
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.\nstate blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.\nbackend blocks are not allowed inside modules. Define the backend at the root configuration level."
        );
        assert_eq!(
            combined
                .diagnostic_lines_relative_to(Some(tmp.path()))
                .iter()
                .filter(|line| matches!(line, super::super::error::DiagnosticLine::RootEvidence(_)))
                .count(),
            2
        );
    }

    #[test]
    fn imported_empty_exports_is_prohibited_by_block_presence() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("arguments.crn"), "arguments {}\n").unwrap();
        fs::write(tmp.path().join("exports.crn"), "exports {}\n").unwrap();
        let loaded = load_module_with_diagnostics(
            &ResolvedPath::new(tmp.path()).unwrap(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .err()
        .expect("exports prohibit import");
        let errors = loaded.into_errors();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].to_string(),
            "exports blocks are not allowed inside modules. Define exports at the root configuration level."
        );
    }

    #[test]
    fn empty_import_diagnostic_names_its_directory_without_display_root() {
        let tmp = tempdir().unwrap();
        let empty = tmp.path().join("empty-module");
        fs::create_dir(&empty).unwrap();
        let error = load_module_with_diagnostics(
            &ResolvedPath::new(&empty).unwrap(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .err()
        .unwrap();
        assert!(matches!(error, ModuleLoadError::NotModule { .. }));
        assert_eq!(
            error.to_string(),
            format!("No .crn files found in {}", empty.display())
        );
    }

    #[test]
    fn empty_import_stays_not_module_when_display_root_is_unavailable() {
        let tmp = tempdir().unwrap();
        let empty = tmp.path().join("empty-module");
        fs::create_dir(&empty).unwrap();
        let error = load_module_with_diagnostics_and_schemas(
            &ResolvedPath::new(&empty).unwrap(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            &crate::schema::SchemaRegistry::new(),
            std::path::Path::new(""),
        )
        .err()
        .unwrap();
        assert!(
            matches!(error, ModuleLoadError::NotModule { .. }),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            format!("No .crn files found in {}", empty.display())
        );
    }

    /// Sibling `.crn` files must be merged in sorted filename order so that
    /// downstream first-match-wins lookups (LSP hover/completion, CLI
    /// diagnostic ordering) are deterministic across filesystems.
    #[test]
    fn load_module_directory_merge_order_is_deterministic() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("z_last.crn"), "arguments {\n  c: String\n}\n").unwrap();
        fs::write(dir.join("a_first.crn"), "arguments {\n  a: String\n}\n").unwrap();
        fs::write(dir.join("m_middle.crn"), "arguments {\n  b: String\n}\n").unwrap();

        let parsed = load_module(
            &crate::paths::ResolvedPath::new(dir).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("module should load");
        let names: Vec<&str> = parsed.arguments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn load_module_diagnostics_cover_arguments_and_internal_bindings() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(
            dir.join("a.crn"),
            "arguments {\n  input: String\n}\nlet internal = \"a\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("b.crn"),
            "arguments {\n  input: String\n}\nlet internal = \"b\"\n",
        )
        .unwrap();

        let loaded = load_module_with_diagnostics(
            &crate::paths::ResolvedPath::new(dir).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("module should load");
        let messages: Vec<String> = loaded
            .duplicate_declarations
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(
            messages,
            vec![
                "duplicate argument 'input': declared in a.crn and b.crn",
                "duplicate binding 'internal': declared in a.crn and b.crn",
            ]
        );
    }

    #[test]
    fn load_module_returns_none_for_file_path() {
        let tmp = tempdir().unwrap();
        let single = tmp.path().join("solo.crn");
        fs::write(&single, "arguments {\n  x: String\n}\n").unwrap();
        assert!(
            load_module(
                &crate::paths::ResolvedPath::new(&single).unwrap(),
                &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
            )
            .is_err()
        );
    }

    #[test]
    fn load_module_from_directory_merges_multiple_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("a.crn"), "arguments {\n  a: String\n}\n").unwrap();
        fs::write(dir.join("b.crn"), "arguments {\n  b: String\n}\n").unwrap();

        let parsed = load_module_from_directory(
            &crate::paths::ResolvedPath::new(dir).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("dir should load");
        assert_eq!(parsed.arguments.len(), 2);
    }

    #[test]
    fn derive_module_name_uses_directory_name_for_directory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("web_tier");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            derive_module_name(&crate::paths::ResolvedPath::new(&dir).unwrap()),
            "web_tier"
        );
    }

    #[test]
    fn derive_module_name_uses_parent_for_main_crn() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("web_tier");
        fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.crn");
        fs::write(&main, "").unwrap();
        assert_eq!(
            derive_module_name(&crate::paths::ResolvedPath::new(&main).unwrap()),
            "web_tier"
        );
    }

    #[test]
    fn derive_module_name_uses_file_stem_for_other_files() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("custom.crn");
        fs::write(&path, "").unwrap();
        assert_eq!(
            derive_module_name(&crate::paths::ResolvedPath::new(&path).unwrap()),
            "custom"
        );
    }
}

#[cfg(test)]
mod importability_tests {
    use super::*;
    #[test]
    fn module_load_distinguishes_unavailable_source_from_rejected_directories() {
        let directory = tempfile::tempdir().unwrap();
        let path = ResolvedPath::new(directory.path()).unwrap();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let Err(ModuleLoadError::NotModule { error, .. }) = load_module(&path, &context) else {
            panic!("empty directory must have a typed rejection");
        };
        assert!(matches!(
            error.into_error().root_cause(),
            ModuleError::NoConfigurationFiles { path: _ }
        ));
        fs::write(directory.path().join("arguments.crn"), "arguments {}\n").unwrap();
        fs::write(directory.path().join("broken.crn"), "let broken =\n").unwrap();
        assert!(matches!(
            load_module(&path, &context),
            Err(ModuleLoadError::Unavailable(ModuleError::ParseInFile {
                path: _,
                error: _
            }))
        ));
        fs::write(directory.path().join("broken.crn"), [0xff]).unwrap();
        assert!(matches!(
            load_module(&path, &context),
            Err(ModuleLoadError::Unavailable(ModuleError::Io(_)))
        ));
        fs::remove_file(directory.path().join("broken.crn")).unwrap();
        fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        let Err(ModuleLoadError::NotModule { error, .. }) = load_module(&path, &context) else {
            panic!("root evidence must prohibit import");
        };
        let error = error.into_error();
        assert!(matches!(
            error.root_cause(),
            ModuleError::ProviderInModule { evidence: _ }
        ));
        assert!(error.root_evidence().is_some());
    }

    #[test]
    fn complete_directory_verdict_includes_require_and_preserves_the_neutral_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = ResolvedPath::new(directory.path()).unwrap();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        fs::write(directory.path().join("main.crn"), "let value = 1\n").unwrap();
        for (evidence, expected) in [
            ("", "neutral"),
            ("arguments {}\n", "module"),
            ("attributes {}\n", "module"),
            ("require false, 'PRIVATE_GUARD'\n", "module"),
            ("provider mock {}\nrequire false, 'PRIVATE_GUARD'\n", "root"),
            (
                "backend local { path = 'state.json' }\narguments {}\n",
                "root",
            ),
        ] {
            fs::write(directory.path().join("evidence.crn"), evidence).unwrap();
            let loaded = load_module_from_directory(&path, &context).unwrap();
            use crate::config_loader::DirectoryVerdict;
            let actual = match loaded
                .directory_verdict()
                .expect("issued directory verdict")
            {
                DirectoryVerdict::Root(_) => "root",
                DirectoryVerdict::ModuleTemplate(_) => "module",
                DirectoryVerdict::NoEvidence(_) => "neutral",
                DirectoryVerdict::Unknown(_) => panic!("complete inventory must issue a verdict"),
            };
            assert_eq!(actual, expected, "{evidence}");
        }
    }
}

#[cfg(test)]
mod imported_snapshot_tests {
    use super::*;
    #[test]
    fn prohibited_snapshot_is_not_importable_but_retains_a_signature_source() {
        for (name, source) in [
            ("exports.crn", "exports { value: Int = 1 }\n"),
            ("state.crn", "removed { from = mock.test.resource 'old' }\n"),
            (
                "upstream.crn",
                "let prior = upstream_state { source = '../prior' }\n",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            fs::write(
                directory.path().join("arguments.crn"),
                "arguments { n: Int = 1 }\n",
            )
            .unwrap();
            fs::write(directory.path().join(name), source).unwrap();
            let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
            assert!(
                load_module(&ResolvedPath::new(directory.path()).unwrap(), &context).is_err(),
                "{name}: a prohibited directory cannot be imported"
            );
            let source = load_module_signature_with_schemas(
                &ResolvedPath::new(directory.path()).unwrap(),
                &context,
                &crate::schema::SchemaRegistry::new(),
                directory.path(),
            )
            .unwrap();
            assert_eq!(source.arguments.len(), 1);
        }
    }
}

#[cfg(test)]
mod signature_view_tests {
    use super::*;
    #[test]
    fn prohibited_signature_view_distinguishes_empty_interface_from_unavailable_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = ResolvedPath::new(directory.path()).unwrap();
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        fs::write(directory.path().join("exports.crn"), "exports {}\n").unwrap();
        let Err(ModuleLoadError::NotModule {
            error,
            imports: _,
            signature,
        }) = load_module(&path, &context)
        else {
            panic!("prohibited parsed directory");
        };
        assert!(matches!(*error, ModuleRejection::Prohibited(_)));
        assert!(matches!(signature, ModuleSignatureView::EmptyInterface));
        fs::write(
            directory.path().join("arguments.crn"),
            "arguments { n: Int = 1 }\n",
        )
        .unwrap();
        let Err(ModuleLoadError::NotModule {
            error,
            imports: _,
            signature,
        }) = load_module(&path, &context)
        else {
            panic!("prohibited parsed directory");
        };
        assert!(matches!(*error, ModuleRejection::Prohibited(_)));
        let ModuleSignatureView::Declarations(interface) = signature else {
            panic!("nonempty interface remains available");
        };
        assert_eq!(interface.arguments[0].name, "n");
    }

    #[test]
    fn enriched_parse_failure_cannot_supply_a_prohibited_signature() {
        for root_evidence in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            fs::write(
                directory.path().join("arguments.crn"),
                "arguments { n: Int = 1 }\n",
            )
            .unwrap();
            fs::write(
                directory.path().join("errors.crn"),
                "attributes { bad: TotallyMadeUpType = 'one' }\n",
            )
            .unwrap();
            fs::write(
                directory.path().join("exports.crn"),
                "exports { value = 'one' }\n",
            )
            .unwrap();
            if root_evidence {
                fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
            }
            let context = ProviderContext {
                custom_types: crate::parser::ProviderCustomTypes::loaded(
                    Default::default(),
                    Box::new(|_, _| Ok(())),
                ),
                ..ProviderContext::new(crate::EnvironmentSnapshot::empty())
            };
            let Err(ModuleLoadError::NotModule {
                error,
                imports: _,
                signature,
            }) = load_module(&ResolvedPath::new(directory.path()).unwrap(), &context)
            else {
                panic!("fallback classifies prohibition");
            };
            assert!(matches!(*error, ModuleRejection::Prohibited(_)));
            assert!(matches!(signature, ModuleSignatureView::Unavailable));
            let path = ResolvedPath::new(directory.path()).unwrap();
            assert!(
                crate::config_loader::parse_directory_files(
                    &[(
                        directory.path().join("errors.crn"),
                        fs::read_to_string(directory.path().join("errors.crn")).unwrap()
                    )],
                    &context
                )
                .is_err()
            );
            assert!(
                crate::config_loader::parse_directory_files(
                    &[(
                        directory.path().join("errors.crn"),
                        fs::read_to_string(directory.path().join("errors.crn")).unwrap()
                    )],
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty())
                )
                .is_ok()
            );
            assert!(
                load_module_signature_with_schemas(
                    &path,
                    &context,
                    &crate::schema::SchemaRegistry::new(),
                    directory.path()
                )
                .is_err(),
                "fallback syntax classifies prohibition but grants no static source"
            );
        }
    }
}
