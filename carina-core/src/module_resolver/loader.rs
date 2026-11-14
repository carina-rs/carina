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
    verdict: crate::config_loader::CompleteDirectoryVerdict,
    pub parsed: ParsedFile,
    pub duplicate_declarations: Vec<crate::config_loader::DuplicateDeclaration>,
}

impl LoadedModule {
    /// Directory selected by the module loader, pinned before parsing its siblings.
    pub fn directory(&self) -> &ResolvedPath {
        &self.directory
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
    },
}

/// Successfully inspected inputs that cannot be imported. The payload cannot
/// contain an unrelated resolver error or grow a silently ignored cause.
#[derive(Debug, Clone)]
pub enum ModuleRejection {
    NotADirectory {
        path: String,
    },
    NoConfigurationFiles {
        path: crate::paths::DisplayPath,
    },
    Root {
        root: crate::config_loader::RootDirectoryVerdict,
        body: Vec<ModuleBodyProhibition>,
    },
}
#[derive(Debug, Clone, Copy)]
pub enum ModuleBodyProhibition {
    State,
    UpstreamState,
    Exports,
}
impl ModuleBodyProhibition {
    fn into_error(self) -> ModuleError {
        match self {
            Self::State => ModuleError::StateBlockInModule,
            Self::UpstreamState => ModuleError::UpstreamStateInModule,
            Self::Exports => ModuleError::ExportsInModule,
        }
    }
}
impl ModuleRejection {
    fn into_nonempty_errors(self) -> (ModuleError, Vec<ModuleError>) {
        match self {
            Self::NotADirectory { path } => (ModuleError::NotADirectory { path }, vec![]),
            Self::NoConfigurationFiles { path } => {
                (ModuleError::NoConfigurationFiles { path }, vec![])
            }
            Self::Root { root, body } => {
                let (first, mut rest) = root.import_prohibitions();
                rest.extend(body.into_iter().map(ModuleBodyProhibition::into_error));
                (first, rest)
            }
        }
    }
    pub fn into_errors(self) -> Vec<ModuleError> {
        let (first, rest) = self.into_nonempty_errors();
        std::iter::once(first).chain(rest).collect()
    }
    fn into_error(self) -> ModuleError {
        let (first, rest) = self.into_nonempty_errors();
        ModuleError::from_nonempty_errors(first, rest)
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
            Self::NotModule { error, imports: _ } => error.into_error(),
        }
    }
    pub fn into_errors(self) -> Vec<ModuleError> {
        match self {
            Self::Unavailable(ModuleError::Multiple(errors)) => errors.into_vec(),
            Self::Unavailable(error) => vec![error],
            Self::NotModule { error, imports: _ } => error.into_errors(),
        }
    }
    fn rejected(error: ModuleRejection) -> Self {
        Self::NotModule {
            error: Box::new(error),
            imports: Vec::new(),
        }
    }
    pub(crate) fn into_first_error(self) -> ModuleError {
        self.into_errors()
            .into_iter()
            .next()
            .expect("a module failure is nonempty")
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
    .map(|loaded| loaded.parsed)
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
    let parsed_files = inputs
        .parse_from_root(config, display_root)
        .map_err(|located| {
            Unavailable(match located.source_file {
                Some(path) => ModuleError::ParseInFile {
                    path,
                    error: located.error,
                },
                None => ModuleError::Parse(*located.error),
            })
        })?;
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
    let module = LoadedModule {
        directory: dir_path.clone(),
        verdict,
        parsed: merged,
        duplicate_declarations,
    };
    // The issued verdict is the importability decision. A root cannot escape
    // rejection even if a public AST list is later changed: the evidence itself
    // still identifies a prohibited root block.
    match module.verdict {
        crate::config_loader::DirectoryVerdict::Root(ref root) => {
            let error = ModuleRejection::Root {
                root: root.clone(),
                body: module.body_prohibitions(),
            };
            return Err(ModuleLoadError::NotModule {
                error: Box::new(error),
                imports: module.parsed.uses,
            });
        }
        // These snapshots remain available to signature/reference checking.
        // State/upstream/exports prohibitions run in validation and expansion,
        // preserving independent diagnostics from an otherwise importable body.
        crate::config_loader::DirectoryVerdict::ModuleTemplate(_)
        | crate::config_loader::DirectoryVerdict::NoEvidence(_) => {}
    }
    let mut module = module;
    crate::parser::retained::judge_configuration(&mut module.parsed, schemas, config);
    Ok(module)
}

impl LoadedModule {
    fn body_prohibitions(&self) -> Vec<ModuleBodyProhibition> {
        [
            crate::validation::validate_no_state_blocks_in_module(self)
                .err()
                .map(|_| ModuleBodyProhibition::State),
            crate::validation::validate_no_upstream_states_in_module(self)
                .err()
                .map(|_| ModuleBodyProhibition::UpstreamState),
            crate::validation::validate_no_exports_in_module(self)
                .err()
                .map(|_| ModuleBodyProhibition::Exports),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
    pub(crate) fn placement_errors(&self) -> Vec<ModuleError> {
        let mut errors = match self.verdict.root() {
            Some(root) => {
                let (first, rest) = root.import_prohibitions();
                std::iter::once(first).chain(rest).collect()
            }
            None => Vec::new(),
        };
        errors.extend(
            self.body_prohibitions()
                .into_iter()
                .map(ModuleBodyProhibition::into_error),
        );
        errors
    }
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
        .map_err(|e| format!("Failed to parse directory {}: {}", dir.display(), e.error))?;

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
            loaded.directory_verdict(),
            crate::config_loader::DirectoryVerdict::Root(_)
        ));
        let schemas = crate::schema::SchemaRegistry::new();
        let judged = crate::config_loader::load_buffer_configuration(&loaded, &schemas, &config);
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

        let parsed = load_module(
            &crate::paths::ResolvedPath::new(dir).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("module should load when arguments are declared");
        assert_eq!(parsed.arguments.len(), 1);
        assert_eq!(parsed.arguments[0].name, "env");
        assert_eq!(parsed.export_params.len(), 1);
        assert_eq!(parsed.export_params[0].name, "region");
    }

    #[test]
    fn imported_root_reports_backend_before_state_and_keeps_each_evidence() {
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
        let errors = error.into_errors();
        assert!(matches!(
            errors[0].root_cause(),
            ModuleError::ProviderInModule
        ));
        assert!(matches!(
            errors[1].root_cause(),
            ModuleError::BackendInModule
        ));
        assert!(matches!(
            errors[2].root_cause(),
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
            errors[1]
                .root_evidence()
                .unwrap()
                .note()
                .contains("b-backend.crn:1.")
        );
        assert_eq!(
            errors[1].root_evidence().unwrap().related_message(),
            "This backend block makes the directory a root configuration."
        );
        let combined = ModuleError::from_errors(errors).unwrap();
        assert!(combined.root_evidence().is_some());
        assert_eq!(
            combined.lsp_message_relative_to(tmp.path()),
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.\nbackend blocks are not allowed inside modules. Define the backend at the root configuration level.\nstate blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level."
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
        .unwrap();
        let errors = loaded.placement_errors();
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
    fn load_module_distinguishes_a_file_path_from_unavailable_source() {
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
            ModuleError::NoConfigurationFiles { .. }
        ));
        fs::write(directory.path().join("arguments.crn"), "arguments {}\n").unwrap();
        fs::write(directory.path().join("broken.crn"), "let broken =\n").unwrap();
        assert!(matches!(
            load_module(&path, &context),
            Err(ModuleLoadError::Unavailable(
                ModuleError::ParseInFile { .. }
            ))
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
        assert!(matches!(error.root_cause(), ModuleError::ProviderInModule));
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
            let actual = match loaded.directory_verdict() {
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
    fn importable_snapshot_keeps_non_root_prohibitions_for_the_validation_phase() {
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
                load_module(&ResolvedPath::new(directory.path()).unwrap(), &context).is_ok(),
                "{name}: a successfully parsed non-root remains available to static signature/type consumers"
            );
        }
    }
}
