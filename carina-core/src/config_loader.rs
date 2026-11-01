//! Configuration loading and .crn file discovery utilities

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::binding_index::BindingNameSet;
use crate::parser::{self, File, ParsedFile, ProviderContext};
use crate::resource::{BindingAddress, ParserPlaceholder};
use crate::schema::SchemaRegistry;

/// The syntactic kind of one named declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeclarationKind {
    Export,
    Argument,
    ModuleAttribute,
    Binding,
    UserFunction,
    ProviderKind,
}

impl DeclarationKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Argument => "argument",
            Self::ModuleAttribute => "module attribute",
            Self::Binding => "binding",
            Self::UserFunction => "function",
            Self::ProviderKind => "provider kind",
        }
    }
}

/// The namespace used to group declarations for uniqueness checks.
/// Arguments are lexical bindings in the parser, so they intentionally share
/// the binding namespace while retaining their own [`DeclarationKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum UniquenessNamespace {
    Export,
    Binding,
    ModuleAttribute,
    UserFunction,
    ProviderKind,
}

impl DeclarationKind {
    fn uniqueness_namespace(self) -> UniquenessNamespace {
        match self {
            Self::Export => UniquenessNamespace::Export,
            Self::Argument | Self::Binding => UniquenessNamespace::Binding,
            Self::ModuleAttribute => UniquenessNamespace::ModuleAttribute,
            Self::UserFunction => UniquenessNamespace::UserFunction,
            Self::ProviderKind => UniquenessNamespace::ProviderKind,
        }
    }
}

/// One declaration occurrence before sibling files are merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationOccurrence {
    kind: DeclarationKind,
    file: PathBuf,
}

impl DeclarationOccurrence {
    pub fn kind(&self) -> DeclarationKind {
        self.kind
    }
}

/// One duplicate declaration group, retaining every contributing source file.
///
/// Paths are relative to the directory whose files were parsed. Repeated paths
/// are intentional: they preserve multiset semantics for declaration kinds
/// whose parser AST keeps same-file duplicates (`exports`, `arguments`, and
/// module `attributes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateDeclaration {
    name: String,
    occurrences: Vec<DeclarationOccurrence>,
}

impl DuplicateDeclaration {
    /// Construct a duplicate group only when at least two declarations exist.
    pub(crate) fn new(name: String, occurrences: Vec<DeclarationOccurrence>) -> Option<Self> {
        (occurrences.len() >= 2).then_some(Self { name, occurrences })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn occurrences(&self) -> &[DeclarationOccurrence] {
        &self.occurrences
    }

    /// Whether this group has an occurrence in the named relative file.
    pub fn occurs_in_file(&self, file_name: &str) -> bool {
        self.kind_in_file(file_name).is_some()
    }

    /// The declaration kind contributed by the named relative file.
    pub fn kind_in_file(&self, file_name: &str) -> Option<DeclarationKind> {
        self.occurrences
            .iter()
            .find(|occurrence| path_matches_file_name(&occurrence.file, file_name))
            .map(|occurrence| occurrence.kind)
    }

    /// First contributing relative file, for diagnostics without source spans.
    pub fn first_file(&self) -> Option<&Path> {
        self.occurrences
            .first()
            .map(|occurrence| occurrence.file.as_path())
    }
}

fn path_matches_file_name(path: &Path, file_name: &str) -> bool {
    path == Path::new(file_name)
        || path
            .file_name()
            .is_some_and(|candidate| candidate == std::ffi::OsStr::new(file_name))
}

fn write_joined(f: &mut fmt::Formatter<'_>, rendered: &[String]) -> fmt::Result {
    match rendered {
        [only] => write!(f, "{only}"),
        [first, second] => write!(f, "{first} and {second}"),
        [first, middle @ .., last] => {
            write!(f, "{first}, ")?;
            for item in middle {
                write!(f, "{item}, ")?;
            }
            write!(f, "and {last}")
        }
        [] => unreachable!("a duplicate group has occurrences"),
    }
}

impl fmt::Display for DuplicateDeclaration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first_kind = self
            .occurrences
            .first()
            .expect("duplicate has a source")
            .kind;
        let homogeneous = self
            .occurrences
            .iter()
            .all(|occurrence| occurrence.kind == first_kind);

        if !homogeneous {
            let mut counts: BTreeMap<(DeclarationKind, &Path), usize> = BTreeMap::new();
            for occurrence in &self.occurrences {
                *counts
                    .entry((occurrence.kind, occurrence.file.as_path()))
                    .or_default() += 1;
            }
            let rendered: Vec<String> = counts
                .into_iter()
                .map(|((kind, file), count)| {
                    if count == 1 {
                        format!("as {} in {}", kind.label(), file.display())
                    } else {
                        format!("as {} {count} times in {}", kind.label(), file.display())
                    }
                })
                .collect();
            write!(f, "duplicate name '{}': declared ", self.name)?;
            return write_joined(f, &rendered);
        }

        let mut counts: BTreeMap<&Path, usize> = BTreeMap::new();
        for occurrence in &self.occurrences {
            *counts.entry(occurrence.file.as_path()).or_default() += 1;
        }

        write!(
            f,
            "duplicate {} '{}': declared ",
            first_kind.label(),
            self.name
        )?;

        if counts.len() == 1 {
            let (file, count) = counts.into_iter().next().expect("duplicate has a source");
            return write!(f, "{count} times in {}", file.display());
        }

        let rendered: Vec<String> = counts
            .into_iter()
            .map(|(file, count)| {
                if count == 1 {
                    file.display().to_string()
                } else {
                    format!("{count} times in {}", file.display())
                }
            })
            .collect();
        write!(f, "in ")?;
        write_joined(f, &rendered)
    }
}

/// A merged view built from the directory parser's per-file output, after its
/// static evaluation. That evaluation folds only fully static expressions and
/// never folds a reference to a resource binding. This view is captured before
/// reference resolution and the snapshot's function-call folding.
///
/// Only the configuration loader can build a directory view because its
/// constructor requires [`DirectoryResolvedFile`] values produced by the
/// directory parser. A standalone source buffer can use [`Self::parse_source`].
/// There is intentionally no constructor from an arbitrary [`ParsedFile`].
///
/// ```compile_fail
/// # fn bypass(parsed: carina_core::parser::ParsedFile) {
/// let _ = carina_core::config_loader::AuthoredBindingView { parsed };
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct AuthoredBindingView {
    parsed: ParsedFile,
}

impl AuthoredBindingView {
    pub(crate) fn from_directory_files(files: &[(PathBuf, DirectoryResolvedFile)]) -> Self {
        let mut parsed = ParsedFile::default();
        for (_, file) in files {
            merge_parsed_file(&mut parsed, file.parsed.clone());
        }
        Self { parsed }
    }

    /// Parse one in-memory source as an authored binding view.
    ///
    /// This is the single-source counterpart to the directory loader. Editor
    /// buffers may lack directory context, but still supply their real file
    /// identity so any authored declarations remain file-backed. It deliberately
    /// runs the same directory parser as the multi-file loading paths.
    pub fn parse_file_source(
        source: &str,
        source_file: &Path,
        config: &ProviderContext,
    ) -> Result<Self, parser::ParseError> {
        let files = vec![(source_file.to_path_buf(), source.to_string())];
        let parsed_files =
            parse_directory_files_located(&files, config).map_err(|located| *located.error)?;
        Ok(Self::from_directory_files(&parsed_files))
    }

    #[cfg(test)]
    pub(crate) fn parse_source(
        source: &str,
        config: &ProviderContext,
    ) -> Result<Self, parser::ParseError> {
        Self::parse_file_source(source, Path::new("test.crn"), config)
    }

    pub(crate) fn parsed(&self) -> &ParsedFile {
        &self.parsed
    }
}

/// The pre-resource-resolution configuration snapshot used for schema-aware
/// alias resolution. Its schema-resolved output also becomes the plan's
/// unresolved, authored-form resources used for let-binding pairing.
///
/// This wrapper fixes the input shape for block-name resolution. Function calls
/// may already be folded in this snapshot; it does not represent the exact
/// authored reference graph. Core's block-name resolver consumes this value and
/// returns its resources as plain resolved resources with no pairing guarantee;
/// carina-cli validation wiring alone turns them into its canonical
/// resource-pair typestate.
#[derive(Debug, Clone)]
pub struct UnresolvedConfigSnapshot {
    parsed: ParsedFile,
}

impl UnresolvedConfigSnapshot {
    /// Wrap a parsed pre-resolution snapshot for schema-aware alias
    /// resolution. Core's block-name resolver returns its resources as plain
    /// resolved resources with no pairing guarantee.
    pub fn from_parsed(parsed: ParsedFile) -> Self {
        Self { parsed }
    }

    pub(crate) fn into_inner(self) -> ParsedFile {
        self.parsed
    }
}

/// Proof that a relative-binding file came from parsing the directory that a
/// caller is intentionally treating as a root configuration.
///
/// The wrapped file is private and there is no production constructor from a
/// caller-built [`File`]. Read-only consumers can borrow it through
/// [`Self::parsed`]; module resolution consumes this witness before applying
/// root scope.
#[derive(Debug, Clone)]
pub struct RootDirectoryParse<E = crate::parser::ParsedExportParam> {
    parsed: File<E>,
}

pub type ParsedRootDirectory = RootDirectoryParse<crate::parser::ParsedExportParam>;
pub type InferredRootDirectory = RootDirectoryParse<crate::parser::InferredExportParam>;

impl<E> RootDirectoryParse<E> {
    fn new(parsed: File<E>) -> Self {
        Self { parsed }
    }

    pub fn parsed(&self) -> &File<E> {
        &self.parsed
    }

    pub fn into_parsed(self) -> File<E> {
        self.parsed
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn from_parsed_for_test(parsed: File<E>) -> Self {
        Self::new(parsed)
    }
}

impl RootDirectoryParse<crate::parser::InferredExportParam> {
    /// Retry export inference with the provider schemas loaded for this root.
    pub fn retry_unchecked_export_inference(
        &mut self,
        schemas: &crate::provider::ProviderLoadedSchemaRegistry,
    ) -> crate::validation::inference::ExportsInferredWithLoadedProviderSchemas<'_> {
        crate::validation::inference::retry_unchecked_export_inference(&mut self.parsed, schemas)
    }

    /// Resolve root-owned type declarations while preserving this witness.
    pub fn resolve_type_exprs_report(
        &mut self,
        config: &ProviderContext,
    ) -> crate::validation::FileTypeResolutionReport {
        crate::validation::resolve_file_type_exprs_report(&mut self.parsed, config, None)
    }

    /// Resolve root-relative resource references while preserving this witness.
    pub fn resolve_resource_refs_with_config(
        &mut self,
        config: &ProviderContext,
    ) -> Result<(), parser::ParseError> {
        parser::resolve_resource_refs_with_config(&mut self.parsed, config)
    }
}

impl<E> RootDirectoryParse<E> {
    pub fn resolve_provider_unresolved_attributes(
        &mut self,
        config: &ProviderContext,
    ) -> Result<(), parser::ParseError> {
        parser::resolve_provider_unresolved_attributes(&mut self.parsed, config)
    }

    pub fn finalize_provider_configs(&mut self) -> Result<(), parser::ParseError> {
        parser::finalize_provider_configs(&mut self.parsed)
    }
}

impl<E> std::ops::Deref for RootDirectoryParse<E> {
    type Target = File<E>;

    fn deref(&self) -> &Self::Target {
        self.parsed()
    }
}

/// Result of loading configuration, includes the file path containing backend block.
///
/// The unresolved alias-resolution snapshot is opaque: callers must pass it
/// through schema-aware block-name resolution instead of extracting raw
/// resources.
///
/// ```compile_fail
/// # fn bypass(loaded: carina_core::config_loader::LoadedConfig) {
/// let _ = loaded.unresolved_parsed.resources;
/// # }
/// ```
///
/// The struct holds non-blocking diagnostics (e.g. deferred for-iterable
/// binding errors) so the caller can decide to keep running the rest of
/// the static-analysis pipeline and report them together with downstream
/// findings (#2102). Blocking errors (bad parse, missing modules) still
/// come back as `Err` from the loader.
pub struct LoadedConfig {
    /// Post-inference file: every export carries its origin and any typed
    /// unchecked reason directly on the parameter.
    pub parsed: InferredRootDirectory,
    /// Values built from the directory parser's per-file output, after its
    /// static evaluation. That evaluation folds only fully static expressions
    /// and never folds a reference to a resource binding. This view is captured
    /// before reference resolution and the snapshot's function-call folding.
    /// Unused-binding validation consumes only this typed view.
    pub authored_bindings: AuthoredBindingView,
    /// Pre-resource-resolution input retained for schema-aware alias
    /// resolution. Its schema-resolved output also becomes the plan's
    /// unresolved, authored-form resources used for let-binding pairing. This
    /// snapshot may contain folded function calls and must not be used as the
    /// authored reference graph.
    pub unresolved_parsed: UnresolvedConfigSnapshot,
    pub backend_file: Option<PathBuf>,
    /// Directory-wide declaration collisions. These are non-blocking at the
    /// load boundary so editor callers retain the parsed tree; validation
    /// commands decide whether to treat them as hard errors.
    pub duplicate_declarations: Vec<DuplicateDeclaration>,
}

impl LoadedConfig {
    /// Cross root binding resolution without expanding module calls.
    pub fn into_root_expanded(self) -> crate::parser::RootBindingResolvedInferredFile {
        crate::module_resolver::expand_root_directory_file(self.parsed.into_parsed())
    }
}

/// A `ParsedFile` produced by `parse_directory_files` after the
/// sibling-let fixed-point loop has converged. Wrapping the parser
/// output in this newtype is the type-level invariant that makes
/// "consume `.variables` from a directory file" require going through
/// the resolver step - a future caller cannot accidentally read raw
/// `Value::Concrete(EnumIdentifier(sibling_name))` placeholders that
/// the single-file `parser::parse(...)` produces for unresolved sibling
/// references.
///
/// Consumers unwrap via `into_inner()` to take ownership of the
/// `ParsedFile`. Test code can use `parsed()` for borrowed inspection.
#[derive(Debug, Clone)]
pub struct DirectoryResolvedFile {
    parsed: ParsedFile,
    /// Names present in this file before sibling bindings were seeded. The
    /// final parse's `variables` map contains sibling seed echoes, so this set
    /// is required to distinguish local value-let declarations from context.
    local_variable_names: HashSet<String>,
}

impl DirectoryResolvedFile {
    #[cfg(test)]
    pub(crate) fn parsed(&self) -> &ParsedFile {
        &self.parsed
    }

    pub fn into_inner(self) -> ParsedFile {
        self.parsed
    }
}

/// Extract directory-wide duplicate declarations from the final per-file
/// parses, before their name-keyed collections are merged.
pub(crate) fn find_duplicate_declarations(
    directory: &Path,
    parsed_files: &[(PathBuf, DirectoryResolvedFile)],
) -> Vec<DuplicateDeclaration> {
    let mut occurrences = BTreeMap::new();

    for (file, resolved) in parsed_files {
        let relative_file = file
            .strip_prefix(directory)
            .unwrap_or(file.as_path())
            .to_path_buf();
        collect_file_declarations(
            &relative_file,
            &resolved.parsed,
            &resolved.local_variable_names,
            &mut occurrences,
        );
    }

    duplicate_groups(occurrences)
}

type DeclarationGroups = BTreeMap<(UniquenessNamespace, String), Vec<DeclarationOccurrence>>;

fn collect_file_declarations(
    relative_file: &Path,
    parsed: &ParsedFile,
    local_variable_names: &HashSet<String>,
    occurrences: &mut DeclarationGroups,
) {
    let mut record = |kind: DeclarationKind, name: &str| {
        occurrences
            .entry((kind.uniqueness_namespace(), name.to_string()))
            .or_default()
            .push(DeclarationOccurrence {
                kind,
                file: relative_file.to_path_buf(),
            });
    };

    // These vectors preserve repeated declarations in one file, so record
    // every element rather than collapsing to a set. Arguments deliberately
    // group into the binding namespace because the parser registers them as
    // lexical bindings; their occurrence kind keeps diagnostics precise.
    for export in &parsed.export_params {
        record(DeclarationKind::Export, &export.name);
    }
    for argument in &parsed.arguments {
        record(DeclarationKind::Argument, &argument.name);
    }
    for attribute in &parsed.attribute_params {
        record(DeclarationKind::ModuleAttribute, &attribute.name);
    }

    // Start from ParsedFile's canonical structural-binding inventory, whose
    // coverage test in parser/ast.rs is the guard against newly added binding
    // kinds silently escaping this directory-level uniqueness check. A single
    // let declaration can appear there and in `variables`; collapse that echo
    // below. Arguments and functions deliberately leave this set because this
    // pass records them above in their parser-defined namespaces.
    let argument_names: HashSet<&str> = parsed
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect();
    let function_names: HashSet<&str> = parsed.user_functions.keys().map(String::as_str).collect();
    let mut binding_names: BTreeSet<&str> = parsed
        .structural_binding_names()
        .into_iter()
        .filter(|name| !argument_names.contains(name) && !function_names.contains(name))
        .collect();
    for name in local_variable_names {
        if parsed.variables.contains_key(name)
            && !binding_names.contains(name.as_str())
            && !argument_names.contains(name.as_str())
        {
            binding_names.insert(name);
        }
    }
    for name in binding_names {
        if name != "_" {
            record(DeclarationKind::Binding, name);
        }
    }

    // The parser already rejects duplicates within one file, but sibling
    // function maps would otherwise retain whichever file merged last.
    for name in parsed.user_functions.keys() {
        record(DeclarationKind::UserFunction, name);
    }

    // Provider configuration is kind-level. Named provider instances reuse a
    // kind's single top-level default and remain ordinary binding declarations.
    for provider in &parsed.providers {
        if provider.is_default() {
            record(DeclarationKind::ProviderKind, &provider.name);
        }
    }
}

fn duplicate_groups(occurrences: DeclarationGroups) -> Vec<DuplicateDeclaration> {
    occurrences
        .into_iter()
        .filter_map(|((_namespace, name), occurrences)| {
            DuplicateDeclaration::new(name, occurrences)
        })
        .collect()
}

/// Find same-file duplicate declarations from an already parsed buffer.
///
/// This is the degraded-mode counterpart to directory extraction for editors:
/// it cannot diagnose cross-file collisions, but it preserves same-block
/// diagnostics when an unrelated sibling fails to parse.
pub fn find_duplicate_declarations_in_parsed_file(
    file_name: &Path,
    parsed: &ParsedFile,
) -> Vec<DuplicateDeclaration> {
    let mut occurrences = BTreeMap::new();
    let local_variable_names = local_variable_names(parsed);
    collect_file_declarations(file_name, parsed, &local_variable_names, &mut occurrences);
    duplicate_groups(occurrences)
}

/// Load configuration from a directory containing .crn files
pub fn load_configuration(path: &Path) -> Result<LoadedConfig, String> {
    load_configuration_with_config(path, &ProviderContext::default(), &SchemaRegistry::new())
}

/// Load configuration from a directory containing .crn files.
///
/// File paths are rejected — pass a directory instead.
pub fn load_configuration_with_config(
    path: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<LoadedConfig, String> {
    if path.is_file() {
        Err(format!("expected directory, got file: {}", path.display()))
    } else if path.is_dir() {
        // Directory mode
        let files = find_crn_files_in_dir(path)?;
        if files.is_empty() {
            return Err(format!("No .crn files found in {}", path.display()));
        }

        // Read every file once and run the directory-aware parse so
        // each file's `ParseContext` is seeded with the binding-name
        // union from sibling `.crn`s. Without this, e.g. an
        // `arguments {}` in `main.crn` would be invisible to `${env}`
        // interpolations in sibling `role.crn` (#2815, #2817).
        let mut file_inputs: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
        for file in &files {
            let content = fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            file_inputs.push((file.clone(), content));
        }
        let parsed_files = match parse_directory_files_located(&file_inputs, config) {
            Ok(v) => v,
            Err(located) => {
                return Err(located.render(path));
            }
        };
        let authored_bindings = AuthoredBindingView::from_directory_files(&parsed_files);
        let duplicate_declarations = find_duplicate_declarations(path, &parsed_files);

        let empty_parsed = ParsedFile::default;
        let mut merged = empty_parsed();
        let mut unresolved_merged = empty_parsed();
        let mut parse_errors: Vec<String> = Vec::new();
        let mut backend_file: Option<PathBuf> = None;

        for (file, resolved) in parsed_files {
            let mut parsed = resolved.into_inner();
            let mut unresolved = parsed.clone();
            if let Err(e) =
                parser::resolve_resource_refs_defer_unknown_functions(&mut parsed, config)
            {
                parse_errors.push(LocatedParseError::in_file(&file, e).render(path));
                continue;
            }

            // The legacy form unrolled both `merge_parsed_file` calls
            // because the match arm needed independent control over
            // backend collision. Now that the match is gone, both
            // merges can delegate to the same helper. Backend collision
            // for `merged` is checked explicitly below (mirroring the
            // legacy "multiple backend blocks defined" error); the
            // unresolved copy only ever stored a single backend, so we
            // strip it before merging to avoid accidentally overwriting
            // `unresolved_merged.backend`.
            unresolved.backend = None;
            merge_parsed_file(&mut unresolved_merged, unresolved);

            let backend = parsed.backend.take();
            merge_parsed_file(&mut merged, parsed);
            if let Some(backend) = backend {
                if merged.backend.is_some() {
                    parse_errors.push(format!(
                        "{}: multiple backend blocks defined",
                        file.display()
                    ));
                } else {
                    merged.backend = Some(backend);
                    backend_file = Some(file.clone());
                }
            }
        }

        if !parse_errors.is_empty() {
            return Err(parse_errors.join("\n"));
        }

        // Resolve references on the merged result so cross-file
        // `ResourceRef`s can see every sibling binding.
        if let Err(e) = parser::resolve_resource_refs_with_config(&mut merged, config) {
            return Err(e.to_string());
        }
        if let Err(e) =
            parser::resolve_snapshot_function_calls_with_config(&mut unresolved_merged, config)
        {
            return Err(e.to_string());
        }

        // `finalize_provider_configs` is intentionally NOT called here.
        // The merged result is still pre-module-expansion, so deferred
        // `default_tags = mod.tags` shapes cannot be resolved yet. The
        // CLI calls finalize after `module_resolver::resolve_modules_with_config`
        // (see `carina-cli/src/commands/mod.rs`); LSP follows the same
        // contract. See #2717.

        // Phase transition: post-resolve, run rhs-driven inference over
        // every `exports { ... }` declaration so downstream consumers
        // retain origin and unchecked provenance per export (#2360 stage 2).
        let inferred = crate::validation::inference::apply_inference(merged, schemas);

        Ok(LoadedConfig {
            parsed: RootDirectoryParse::new(inferred),
            authored_bindings,
            unresolved_parsed: UnresolvedConfigSnapshot::from_parsed(unresolved_merged),
            backend_file,
            duplicate_declarations,
        })
    } else {
        Err(format!("Path not found: {}", path.display()))
    }
}

/// Parse all `.crn` files in a directory and return a merged `ParsedFile`.
///
/// This is the canonical directory-scope parse: each file is parsed
/// individually, results are merged, and cross-file references are
/// resolved against the combined binding map. Both CLI and LSP should
/// use this to ensure consistent results.
pub fn parse_directory(
    dir: &Path,
    config: &ProviderContext,
) -> Result<ParsedRootDirectory, String> {
    parse_directory_with_overrides(dir, config, &HashMap::new())
}

/// Like [`parse_directory`] but takes an `overrides` map from **file name**
/// (e.g. `"main.crn"`) to source text. Files present in the map are parsed
/// from the override text; the rest are read from disk.
///
/// LSP uses this to analyze the open document's in-memory buffer alongside
/// the on-disk siblings, so edits show diagnostics before the user saves.
pub fn parse_directory_with_overrides(
    dir: &Path,
    config: &ProviderContext,
    overrides: &HashMap<String, String>,
) -> Result<ParsedRootDirectory, String> {
    parse_directory_with_overrides_impl(dir, config, overrides, false)
        .map(|result| result.parsed)
        .map_err(|error| error.to_string())
}

/// Directory parse result for editor and other non-blocking consumers that
/// need the merged AST together with load-seam diagnostics.
pub struct DirectoryParseResult {
    pub parsed: ParsedRootDirectory,
    /// A merged view of the directory parser's per-file output, after its
    /// static evaluation and before reference resolution.
    pub authored_bindings: AuthoredBindingView,
    pub duplicate_declarations: Vec<DuplicateDeclaration>,
    /// Exact source inputs used for the parse, including in-memory overrides.
    pub source_files: Vec<(PathBuf, String)>,
    /// Source file that owns the backend when the directory has exactly one.
    /// `None` means the directory has either zero or multiple backend blocks.
    pub sole_backend_file: Option<PathBuf>,
    /// Per-file ASTs after directory seed resolution, retained for editor
    /// diagnostics that need the correctly classified value while anchoring a
    /// message in one authored file. Empty on non-diagnostic load paths.
    semantic_files: Vec<(PathBuf, ParsedFile)>,
}

/// A directory parse failure with source ownership retained for editor
/// diagnostics.
///
/// CLI-facing loaders continue to render this as the same string they
/// historically returned. Editors can additionally inspect [`Self::source_file`]
/// and only publish parser errors owned by the document being analyzed.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryParseFailure {
    #[error("{0}")]
    Message(String),
    #[error("{location}: {error}")]
    Parse {
        location: PathBuf,
        source_file: Option<PathBuf>,
        error: Box<parser::ParseError>,
    },
}

impl DirectoryParseFailure {
    fn from_parse_error(error: parser::ParseError) -> Self {
        match error {
            parser::ParseError::Located { path, error } => Self::Parse {
                location: path.clone(),
                source_file: Some(path),
                error,
            },
            error => Self::Message(error.to_string()),
        }
    }

    pub fn source_file(&self) -> Option<&Path> {
        match self {
            Self::Parse { source_file, .. } => source_file.as_deref(),
            Self::Message(_) => None,
        }
    }

    pub fn parse_error(&self) -> Option<&parser::ParseError> {
        match self {
            Self::Parse { error, .. } => Some(error.as_ref()),
            Self::Message(_) => None,
        }
    }
}

impl DirectoryParseResult {
    pub fn semantic_file(&self, path: &Path) -> Option<&ParsedFile> {
        self.semantic_files
            .iter()
            .find_map(|(candidate, parsed)| (candidate == path).then_some(parsed))
    }

    /// Cross root binding resolution for the merged directory parse.
    pub fn root_expanded(&self) -> crate::parser::RootBindingResolvedParsedFile {
        crate::module_resolver::expand_root_directory_file(self.parsed.parsed().clone())
    }

    /// Cross root binding resolution for one real file retained by this parse.
    pub fn root_expanded_semantic_file(
        &self,
        path: &Path,
    ) -> Option<crate::parser::RootBindingResolvedParsedFile> {
        self.semantic_file(path)
            .cloned()
            .map(crate::module_resolver::expand_root_directory_file)
    }
}

/// Diagnostic-preserving variant of [`parse_directory_with_overrides`].
///
/// Duplicate declarations do not make the parse fail: LSP callers need the
/// merged tree to survive while a user is editing a collision.
pub fn parse_directory_with_overrides_and_diagnostics(
    dir: &Path,
    config: &ProviderContext,
    overrides: &HashMap<String, String>,
) -> Result<DirectoryParseResult, DirectoryParseFailure> {
    parse_directory_with_overrides_impl(dir, config, overrides, true)
}

fn parse_directory_with_overrides_impl(
    dir: &Path,
    config: &ProviderContext,
    overrides: &HashMap<String, String>,
    preserve_failed_user_calls: bool,
) -> Result<DirectoryParseResult, DirectoryParseFailure> {
    let files = find_crn_files_in_dir(dir).map_err(DirectoryParseFailure::Message)?;
    let mut paths: Vec<(std::path::PathBuf, String)> = files
        .into_iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str().map(|s| (p.clone(), s.to_string())))
        })
        .collect();
    // Also include overrides whose filename isn't on disk yet (e.g. a new
    // buffer the user hasn't saved). Keep the list de-duplicated by name.
    for name in overrides.keys() {
        if !paths.iter().any(|(_, n)| n == name) {
            paths.push((dir.join(name), name.clone()));
        }
    }
    if paths.is_empty() {
        return Err(DirectoryParseFailure::Message(format!(
            "No .crn files found in {}",
            dir.display()
        )));
    }

    // Resolve every file's source text (disk or override) up front so
    // the directory-aware parse helper sees the right inputs. #2817.
    let mut file_inputs: Vec<(PathBuf, String)> = Vec::with_capacity(paths.len());
    for (file, name) in &paths {
        let content = match overrides.get(name) {
            Some(buffer) => buffer.clone(),
            None => fs::read_to_string(file).map_err(|e| {
                DirectoryParseFailure::Message(format!("Failed to read {}: {}", file.display(), e))
            })?,
        };
        file_inputs.push((file.clone(), content));
    }

    let parsed_files = match parse_directory_files_located(&file_inputs, config) {
        Ok(v) => v,
        Err(located) => {
            let location = located
                .source_file
                .clone()
                .unwrap_or_else(|| dir.to_path_buf());
            return Err(DirectoryParseFailure::Parse {
                location,
                source_file: located.source_file,
                error: located.error,
            });
        }
    };
    let authored_bindings = AuthoredBindingView::from_directory_files(&parsed_files);
    let duplicate_declarations = find_duplicate_declarations(dir, &parsed_files);
    let semantic_files = if preserve_failed_user_calls {
        parsed_files
            .iter()
            .map(|(path, resolved)| {
                let mut semantic = resolved.parsed.clone();
                // Pass 2 echoes directory seeds into every file's variable
                // map. They provide parse context but are not authored by this
                // file, so do not evaluate a sibling's declaration under this
                // file's diagnostic location.
                semantic
                    .variables
                    .retain(|name, _| resolved.local_variable_names.contains(name));
                semantic
                    .parser_binding_metadata
                    .retain(|name| resolved.local_variable_names.contains(name.as_str()));
                parser::resolve_resource_refs_for_diagnostics(&mut semantic, config).map_err(
                    |error| {
                        let located = LocatedParseError::in_file(path, error);
                        DirectoryParseFailure::Parse {
                            location: located.source_file.clone().unwrap_or_else(|| path.clone()),
                            source_file: located.source_file,
                            error: located.error,
                        }
                    },
                )?;
                Ok((path.clone(), semantic))
            })
            .collect::<Result<Vec<_>, DirectoryParseFailure>>()?
    } else {
        Vec::new()
    };

    let mut merged = ParsedFile::default();
    let mut saw_backend = false;
    let mut sole_backend_file = None;
    for (file, resolved) in parsed_files {
        let parsed = resolved.into_inner();
        if parsed.backend.is_some() {
            if saw_backend {
                sole_backend_file = None;
            } else {
                saw_backend = true;
                sole_backend_file = Some(file.clone());
            }
        }
        merge_parsed_file(&mut merged, parsed);
    }

    // Resolve cross-file references on the merged result
    let resolve_result = if preserve_failed_user_calls {
        parser::resolve_resource_refs_for_diagnostics(&mut merged, config)
    } else {
        parser::resolve_resource_refs_with_config(&mut merged, config)
    };
    if let Err(error) = resolve_result {
        return Err(DirectoryParseFailure::from_parse_error(error));
    }
    // `finalize_provider_configs` is intentionally NOT called here. The
    // merged result is pre-module-expansion; deferred provider
    // attributes that reference module-call bindings cannot be resolved
    // until `module_resolver::resolve_modules_with_config` runs.
    // Consumers (CLI, LSP) call finalize themselves after expansion.
    // See #2717.

    // Identifier-scope and provider-routing validation are left to callers.
    // LSP needs the ParsedFile even when a reference is unresolved, so it can
    // surface the error as a diagnostic; CLI commands run both checks through
    // the shared validation pipeline.
    Ok(DirectoryParseResult {
        parsed: RootDirectoryParse::new(merged),
        authored_bindings,
        duplicate_declarations,
        source_files: file_inputs,
        sole_backend_file,
        semantic_files,
    })
}

/// Re-label a parsed file's export-param phase (`File<A>` → `File<B>`)
/// when it carries **no** export params.
///
/// Module expansion produces a concrete `ParsedFile` contribution (it
/// is a parser-phase operation), but the caller's target is a generic
/// `File<E>` (`resolve_modules_with_config<E>`). Today every
/// production caller instantiates `E = ParsedExportParam`, so this is
/// a same-phase no-op; the `<A, B>` generality keeps
/// `resolve_modules_with_config` phase-agnostic for a future
/// inferred-phase caller without re-introducing a hand-listed field
/// list. A module contribution never re-exports raw export params —
/// `export_params` is always empty — so the relabel is total and
/// lossless.
///
/// Delegates to [`File::map_export_params`](crate::parser::File), the
/// single exhaustive-destructure phase-axis surface (the carina#3126 /
/// carina#3061 compile-time forcing function: a new `File<E>` field
/// stops `map_export_params` from compiling until classified, so the
/// module-expansion bridge can never silently drop a field).
/// `debug_assert!`s `export_params` is empty — a contract tripwire for
/// a future caller that violates the precondition; not reachable from
/// the sole current caller (`expand_module_call` pins
/// `export_params: Vec::new()`).
pub(crate) fn relabel_export_phase<A, B, C: BindingAddress>(
    f: File<
        A,
        ParserPlaceholder,
        ParserPlaceholder,
        crate::parser::ParsedDeferredForPhase<ParserPlaceholder, C>,
        C,
    >,
) -> File<
    B,
    ParserPlaceholder,
    ParserPlaceholder,
    crate::parser::ParsedDeferredForPhase<ParserPlaceholder, C>,
    C,
> {
    f.map_export_params(|export_params| {
        // Tripwire for a future caller that violates the precondition.
        // `debug_assert!` (not `assert!`): carina-core is a library;
        // the real enforcement is the exhaustive destructure in
        // `map_export_params`, and the sole current caller pins
        // `export_params: Vec::new()`, so this is unreachable today —
        // a release-build crash here would be a strictly worse failure
        // mode than the test/CI catch a debug assert already gives.
        debug_assert!(
            export_params.is_empty(),
            "relabel_export_phase: only an export-param-free file may cross \
             phases; a module contribution must never carry export_params"
        );
        Vec::new()
    })
}

/// Fold one parsed file's content into another (the sibling-`.crn`
/// directory merge, and the module-load merge).
///
/// [`File::merge_from`] performs the exhaustive private-field-preserving
/// merge. Keeping this wrapper generic over the export-param phase lets the
/// parser and inferred phases share that single merge contract.
pub(crate) fn merge_parsed_file<E, B: BindingAddress>(
    target: &mut File<
        E,
        ParserPlaceholder,
        ParserPlaceholder,
        crate::parser::ParsedDeferredForPhase<ParserPlaceholder, B>,
        B,
    >,
    source: File<
        E,
        ParserPlaceholder,
        ParserPlaceholder,
        crate::parser::ParsedDeferredForPhase<ParserPlaceholder, B>,
        B,
    >,
) {
    target.merge_from(source);
}

/// Parse every `(path, source)` pair as part of a single directory unit.
///
/// Directory-scoped implementation that addresses the sibling-scope
/// class (#2817, #3394):
///
/// - **Scope pass** collects top-level `let` names from the grammar without
///   evaluating their right-hand sides.
/// - **Pass 1.a** parses every input with those names installed as provisional
///   references whose value/structural kind is not known yet. The result is used only to classify the real
///   directory-wide declarations; its values are discarded because sibling
///   lets have not reached their fixed point yet.
/// - **Value fixed point** repeatedly parses every input with the current
///   directory seeds until the locally declared `let` values stop
///   changing. Each round accumulates only the per-file local variables,
///   not full `ParsedFile`s, because the loop only feeds seed
///   collection.
/// - **Pass 2** parses every input one final time with the converged
///   union. Downstream callers receive files parsed against the fully
///   resolved directory binding scope, including value-shaped sibling
///   lets and structural placeholders.
///
/// The returned vector preserves the input order. Any per-file parse
/// error short-circuits and is returned to the caller; earlier passes
/// swallow nothing. Pass 2 also stamps each returned declaration with its
/// source file, completing parser-authored spans through the same path for
/// configuration loading, module resolution, and editor diagnostics.
///
/// A K-hop sibling-let chain costs roughly (K+2) full directory parses
/// (Pass 1.a + K fixed-point iterations + Pass 2). Each call to
/// `parse_with_seeded_bindings` with an empty seed list is a no-op in
/// `seed_bindings`, so single-file callers (parser tests, the
/// `parse(input, config)` wrapper) pay zero overhead.
#[derive(Clone, PartialEq)]
struct DirectorySeedState {
    variables: IndexMap<String, parser::ParsedValue>,
    metadata: crate::parser::ParserBindingMetadata,
}

#[derive(Debug)]
pub(crate) struct LocatedParseError {
    pub(crate) source_file: Option<PathBuf>,
    pub(crate) error: Box<parser::ParseError>,
}

impl LocatedParseError {
    fn in_file(path: &Path, error: parser::ParseError) -> Self {
        match error {
            parser::ParseError::Located { path, error } => Self {
                source_file: Some(path),
                error,
            },
            error => Self {
                source_file: Some(path.to_path_buf()),
                error: Box::new(error),
            },
        }
    }

    fn directory_wide(error: parser::ParseError) -> Self {
        match error {
            parser::ParseError::Located { path, error } => Self {
                source_file: Some(path),
                error,
            },
            error => Self {
                source_file: None,
                error: Box::new(error),
            },
        }
    }

    fn render(&self, fallback: &Path) -> String {
        let location = self.source_file.as_deref().unwrap_or(fallback);
        format!("{}: {}", location.display(), self.error)
    }
}

pub fn parse_directory_files(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
) -> Result<Vec<(PathBuf, DirectoryResolvedFile)>, parser::ParseError> {
    // This lower-level API historically returned the leaf parse error. CLI
    // and LSP callers use the located variant below when rendering a file.
    parse_directory_files_located(files, config).map_err(|located| *located.error)
}

pub(crate) fn parse_directory_files_located(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
) -> Result<Vec<(PathBuf, DirectoryResolvedFile)>, LocatedParseError> {
    let display_root = files
        .first()
        .and_then(|(path, _)| path.parent())
        .unwrap_or_else(|| Path::new(""));
    parse_directory_files_located_from_root(files, config, display_root)
}

pub(crate) fn parse_directory_files_located_from_root(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
    display_root: &Path,
) -> Result<Vec<(PathBuf, DirectoryResolvedFile)>, LocatedParseError> {
    let directory_functions = parser::top_level_user_functions_in_files(files, config)
        .map_err(LocatedParseError::directory_wide)?;
    let mut directory_module_parameters = HashSet::new();
    for (path, content) in files {
        directory_module_parameters.extend(
            parser::top_level_argument_names(content)
                .map_err(|error| LocatedParseError::in_file(path, error))?,
        );
    }

    // Establish lexical scope without evaluating any RHS. Postfix access on a
    // sibling name can otherwise fail on the identifier's enum fallback before
    // the value fixed point gets a chance to type the name.
    let mut provisional_names = Vec::new();
    for (path, content) in files {
        provisional_names.extend(
            parser::top_level_let_binding_names(content)
                .map_err(|error| LocatedParseError::in_file(path, error))?,
        );
    }
    provisional_names.sort_unstable();
    provisional_names.dedup();
    let mut provisional_seeds: Vec<_> = provisional_names
        .iter()
        .filter(|name| !directory_module_parameters.contains(name.as_str()))
        .map(|name| parser::BindingSeed::provisional(name))
        .collect();
    provisional_seeds.extend(
        directory_module_parameters
            .iter()
            .map(|name| parser::BindingSeed::module_parameter(name)),
    );

    // Pass 1.a: classify declarations with the complete syntactic scope.
    // Deferred-loop body errors are explicitly retryable while sibling
    // bindings are provisional; every other error propagates here, and the
    // final pass reparses those bodies with fully classified bindings.
    let mut pass1_union = ParsedFile::default();
    let mut pass_1a_local_variables = Vec::with_capacity(files.len());
    for (path, content) in files {
        let parsed = parser::parse_with_directory_context(
            content,
            path,
            config,
            &provisional_seeds,
            &directory_functions,
            false,
        )
        .map_err(|error| LocatedParseError::in_file(path, error))?;
        pass_1a_local_variables.push(local_variable_names(&parsed));
        merge_parsed_file(&mut pass1_union, parsed);
    }
    let structural = pass1_union.structural_binding_names();
    let module_parameters: HashSet<&str> = pass1_union
        .arguments
        .iter()
        .map(|argument| argument.name.as_str())
        .collect();
    parser::reject_cyclic_let_bindings_in_variables(&pass1_union.variables, &structural)
        .map_err(LocatedParseError::directory_wide)?;
    let mut seed_metadata = pass1_union.parser_binding_metadata.clone();
    seed_metadata.clear_pending_function_type_checks();
    let mut bindings_current = DirectorySeedState {
        variables: pass1_union.variables.clone(),
        metadata: seed_metadata,
    };
    let iteration_cap = pass1_union.variables.len() + 1;
    let mut converged = false;

    for _ in 0..iteration_cap {
        let seeds = parser::collect_seed_bindings_from_parts(
            &structural,
            &module_parameters,
            &bindings_current.variables,
            &bindings_current.metadata,
        );
        let bindings_next = parse_local_variable_union(
            files,
            config,
            &seeds,
            &pass_1a_local_variables,
            &directory_functions,
        )?;
        parser::reject_cyclic_let_bindings_in_variables(&bindings_next.variables, &structural)
            .map_err(LocatedParseError::directory_wide)?;

        if bindings_next == bindings_current {
            converged = true;
            break;
        }
        bindings_current = bindings_next;
    }

    // Paranoid backstop only. The fixed-point sequence is monotone for
    // parser-produced seeds, so convergence should occur in K iterations
    // where K is the longest sibling-let chain. This path has no direct
    // test because constructing non-monotone seed input requires bypassing
    // the parser internals.
    if !converged {
        return Err(LocatedParseError::directory_wide(
            parser::ParseError::InternalError {
                expected: "let resolution fixed point".to_string(),
                context: format!(
                    "let resolution did not converge after {iteration_cap} iterations; \
                     this is a bug - please file an issue with the offending directory"
                ),
            },
        ));
    }

    let final_seeds = parser::collect_seed_bindings_from_parts(
        &structural,
        &module_parameters,
        &bindings_current.variables,
        &bindings_current.metadata,
    );

    // Pass 2: re-parse each file with the sibling-aware union seeded into `ctx`.
    let mut out = Vec::with_capacity(files.len());
    for ((path, content), local_variable_names) in files.iter().zip(pass_1a_local_variables) {
        let mut parsed = parser::parse_with_directory_context(
            content,
            path,
            config,
            &final_seeds,
            &directory_functions,
            true,
        )
        .map_err(|error| LocatedParseError::in_file(path, error))?;
        record_parsed_file_source_metadata(&mut parsed, path, display_root);
        out.push((
            path.clone(),
            DirectoryResolvedFile {
                parsed,
                local_variable_names,
            },
        ));
    }
    let mut merged = ParsedFile::default();
    for (_, file) in &out {
        merge_parsed_file(&mut merged, file.parsed.clone());
    }
    let binding_scope = BindingNameSet::from_parsed(&merged)
        .iter_names()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    for (_, file) in &mut out {
        file.parsed.set_loop_binding_scope(binding_scope.clone());
    }
    Ok(out)
}

fn record_parsed_file_source_metadata(parsed: &mut ParsedFile, path: &Path, display_root: &Path) {
    parsed
        .parser_binding_metadata
        .set_pending_function_source_file(path);
    let resource_ids = parsed
        .resources
        .iter()
        .map(|resource| resource.id.clone())
        .chain(
            parsed
                .data_sources
                .iter()
                .map(|resource| resource.id.clone()),
        )
        .chain(
            parsed
                .parsed_deferred_for_expressions()
                .iter()
                .filter_map(|deferred| match deferred.body() {
                    crate::parser::DeferredForBody::Resource(resource) => {
                        Some(resource.id().clone())
                    }
                    crate::parser::DeferredForBody::Read(data_source) => {
                        Some(data_source.id.clone())
                    }
                    crate::parser::DeferredForBody::ModuleCall(_) => None,
                }),
        )
        .chain(
            parsed
                .deferred_module_ifs
                .iter()
                .flat_map(|deferred| deferred.branches())
                .filter_map(|branch| match branch {
                    parser::DeferredIfBody::Resource(resource) => Some(resource.id.clone()),
                    parser::DeferredIfBody::DataSource(data_source) => Some(data_source.id.clone()),
                    parser::DeferredIfBody::ModuleCall(_) => None,
                }),
        )
        .collect::<Vec<_>>();
    for id in resource_ids {
        parsed
            .parser_binding_metadata
            .insert_resource_source_file(id, path.to_path_buf());
    }
    for function in parsed.user_functions.values_mut() {
        function.body.source_file = Some(path.to_path_buf());
    }
    let display_file = path
        .strip_prefix(display_root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| relative.to_string_lossy().into_owned())
        .or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| path.display().to_string());
    for resource in &mut parsed.resources {
        resource.loop_provenance.set_display_file(&display_file);
    }
    for call in &mut parsed.module_calls {
        if let Some(origin) = call.loop_origin_mut() {
            origin.set_display_file(&display_file);
        }
    }
    for deferred in parsed.parsed_deferred_for_expressions_mut() {
        deferred.set_loop_display_file(&display_file);
        if let Some(provenance) = deferred.template_loop_provenance_mut() {
            provenance.set_display_file(&display_file);
        }
    }
    for deferred in &mut parsed.deferred_module_ifs {
        for branch in deferred.branches_mut() {
            match branch {
                parser::DeferredIfBody::Resource(resource) => {
                    resource.loop_provenance.set_display_file(&display_file);
                }
                parser::DeferredIfBody::DataSource(_) => {}
                parser::DeferredIfBody::ModuleCall(call) => {
                    if let Some(origin) = call.loop_origin_mut() {
                        origin.set_display_file(&display_file);
                    }
                }
            }
        }
    }

    let warning_file = Some(path.display().to_string());
    for warning in &mut parsed.warnings {
        warning.file = warning_file.clone();
    }
    for deferred in parsed.parsed_deferred_for_expressions_mut() {
        deferred.record_source_location(path, display_root);
    }
}

fn local_variable_names(parsed: &ParsedFile) -> HashSet<String> {
    parsed
        .variables
        .keys()
        .map(String::as_str)
        .chain(
            parsed
                .parser_binding_metadata
                .structural_handles()
                .keys()
                .map(crate::resource::BindingAddress::as_str),
        )
        .map(str::to_owned)
        .collect()
}

fn parse_local_variable_union(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
    seeds: &[parser::BindingSeed<'_>],
    local_variables_by_file: &[HashSet<String>],
    user_functions: &HashMap<String, parser::UserFunction>,
) -> Result<DirectorySeedState, LocatedParseError> {
    let mut variables = IndexMap::new();
    let mut metadata = crate::parser::ParserBindingMetadata::default();
    for ((path, content), local_variables) in files.iter().zip(local_variables_by_file.iter()) {
        let mut parsed = parser::parse_with_directory_context(
            content,
            path,
            config,
            seeds,
            user_functions,
            false,
        )
        .map_err(|error| LocatedParseError::in_file(path, error))?;
        parsed
            .variables
            .retain(|name, _| local_variables.contains(name));
        parsed
            .parser_binding_metadata
            .retain(|name| local_variables.contains(name.as_str()));
        parsed
            .parser_binding_metadata
            .clear_pending_function_type_checks();
        variables.extend(parsed.variables);
        metadata.extend(parsed.parser_binding_metadata);
    }
    Ok(DirectorySeedState {
        variables,
        metadata,
    })
}

/// Get base directory for module resolution.
///
/// Since paths are now always directories, this returns the path as-is.
/// Kept for backward compatibility with callers.
pub fn get_base_dir(path: &Path) -> &Path {
    path
}

/// Find all .crn files recursively in a directory, skipping hidden dirs,
/// target, and node_modules.
///
/// The returned paths are sorted lexicographically by `PathBuf` ordering so
/// callers get deterministic results across filesystems (#2851). This matches
/// the contract of [`find_crn_files_in_dir`].
pub fn find_crn_files_recursive(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_crn_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_crn_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    // Sort directory entries before descending so traversal order is
    // independent of the underlying filesystem's `readdir` order.
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    for path in paths {
        if path.is_dir() {
            // Skip hidden directories and common non-source directories
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_crn_files_recursive(&path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }

    Ok(())
}

/// Find .crn files in a single directory (non-recursive).
///
/// The returned paths are sorted by `PathBuf` ordering, which for a
/// single-directory listing degenerates to filename order. Callers that
/// walk siblings independently (rather than going through
/// `parse_directory`'s merge path) get deterministic order across
/// filesystems (#2449).
pub fn find_crn_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ModuleCall;
    use crate::resource::{DeferredValue, Value};
    use std::fs;

    /// Helper to create a temporary directory for tests
    fn create_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("carina_config_loader_test_{}", name));
        // Clean up if it exists from a previous run
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Helper to clean up a temporary directory
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_configuration_preserves_pest_syntax_diagnostic() {
        let dir = create_temp_dir("syntax_diagnostic");
        fs::write(dir.join("broken.crn"), "let =\n").unwrap();

        let error = match load_configuration(&dir) {
            Ok(_) => panic!("invalid syntax unexpectedly loaded"),
            Err(error) => error,
        };

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
        cleanup(&dir);
    }

    // ========== find_crn_files_in_dir tests ==========

    #[test]
    fn find_crn_files_in_dir_empty_directory() {
        let dir = create_temp_dir("in_dir_empty");
        let result = find_crn_files_in_dir(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_with_crn_files() {
        let dir = create_temp_dir("in_dir_with_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.crn"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].ends_with("a.crn"));
        assert!(result[1].ends_with("b.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_returns_paths_sorted_by_name() {
        // #2449: callers that walk siblings independently (rather than
        // through `parse_directory`'s merge path) used to inherit the
        // filesystem-dependent order. Pin lexicographic order so first-
        // match-wins lookups are deterministic across ext4 / APFS / tmpfs.
        // Insert the files in reverse name order to defeat the common
        // "creation order == listing order" fallback on tmpfs.
        let dir = create_temp_dir("in_dir_sorted");
        for name in ["z.crn", "m.crn", "a.crn", "providers.crn", "main.crn"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let result = find_crn_files_in_dir(&dir).unwrap();
        let names: Vec<String> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a.crn", "m.crn", "main.crn", "providers.crn", "z.crn"]
        );
        cleanup(&dir);
    }

    // #2465: pin the signature as `&Path`-accepting at compile time, so a
    // future revert to `&PathBuf` fails to build instead of silently
    // re-introducing the `.to_path_buf()` ceremony at every LSP callsite.
    const _: fn(&Path) -> Result<Vec<PathBuf>, String> = find_crn_files_in_dir;

    #[test]
    fn find_crn_files_in_dir_ignores_non_crn_files() {
        let dir = create_temp_dir("in_dir_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.rs"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_does_not_recurse() {
        let dir = create_temp_dir("in_dir_no_recurse");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("top.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_dir_xyz");
        let result = find_crn_files_in_dir(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    // ========== find_crn_files_recursive tests ==========

    #[test]
    fn find_crn_files_recursive_empty_directory() {
        let dir = create_temp_dir("recursive_empty");
        let result = find_crn_files_recursive(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_finds_nested_files() {
        let dir = create_temp_dir("recursive_nested");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();
        let deep = sub.join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.crn"), "").unwrap();

        let mut result = find_crn_files_recursive(&dir).unwrap();
        result.sort();
        assert_eq!(result.len(), 3);
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_hidden_directories() {
        let dir = create_temp_dir("recursive_hidden");
        fs::write(dir.join("visible.crn"), "").unwrap();
        let hidden = dir.join(".hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("secret.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("visible.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_target_directory() {
        let dir = create_temp_dir("recursive_target");
        fs::write(dir.join("main.crn"), "").unwrap();
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("build.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("main.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_node_modules() {
        let dir = create_temp_dir("recursive_node_modules");
        fs::write(dir.join("app.crn"), "").unwrap();
        let nm = dir.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("dep.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("app.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_ignores_non_crn_files() {
        let dir = create_temp_dir("recursive_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.json"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_recursive_nonexistent_xyz");
        let result = find_crn_files_recursive(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    /// Regression test for #2851: recursive .crn discovery must return
    /// paths in lexicographic order regardless of the underlying
    /// filesystem's `readdir` order. We create the files in
    /// reverse-lexicographic order to make non-deterministic ordering
    /// more likely to surface on filesystems that return entries in
    /// creation order.
    #[test]
    fn find_crn_files_recursive_returns_paths_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create sibling files and nested directories in reverse order.
        let sub_z = root.join("z_sub");
        let sub_a = root.join("a_sub");
        fs::create_dir_all(&sub_z).unwrap();
        fs::create_dir_all(&sub_a).unwrap();

        fs::write(sub_z.join("z.crn"), "").unwrap();
        fs::write(sub_z.join("a.crn"), "").unwrap();
        fs::write(sub_a.join("z.crn"), "").unwrap();
        fs::write(sub_a.join("a.crn"), "").unwrap();
        fs::write(root.join("z_top.crn"), "").unwrap();
        fs::write(root.join("a_top.crn"), "").unwrap();

        let result = find_crn_files_recursive(root).unwrap();

        let mut expected = result.clone();
        expected.sort();
        assert_eq!(
            result, expected,
            "find_crn_files_recursive must return paths in lexicographic order"
        );
        assert_eq!(result.len(), 6);
    }

    // ========== get_base_dir tests ==========

    #[test]
    fn get_base_dir_returns_path_as_is() {
        let dir = create_temp_dir("base_dir_as_is");

        let base = get_base_dir(&dir);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_directory() {
        let dir = create_temp_dir("base_dir_directory");

        let base = get_base_dir(&dir);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_nonexistent_path() {
        // Non-existent path is neither file nor dir, so returns the path itself
        let path = Path::new("/tmp/carina_nonexistent_path_xyz");
        let base = get_base_dir(path);
        assert_eq!(base, path);
    }

    // ========== load_configuration tests ==========

    #[test]
    fn load_configuration_runs_inference_and_surfaces_errors() {
        // An unannotated dynamic-rhs export (`lookup` returns Any) carries
        // its inference failure on the export parameter.
        let dir = create_temp_dir("load_inference_failure");
        fs::write(
            dir.join("main.crn"),
            "exports {\n  zone_id = lookup({a = \"1\"}, \"a\", \"default\")\n}\n",
        )
        .unwrap();

        let loaded = load_configuration_with_config(
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .unwrap();
        cleanup(&dir);
        assert_eq!(loaded.parsed.export_params.len(), 1);
        assert!(matches!(
            &loaded.parsed.export_params[0],
            crate::parser::InferredExportParam {
                name,
                static_type: crate::static_type::ExportStaticType::Inferred(
                    crate::static_type::StaticType::Unchecked(
                        crate::static_type::InferredUncheckedReason::InferenceFailed(_)
                    )
                ),
                ..
            } if name == "zone_id"
        ));
    }

    #[test]
    fn load_configuration_keeps_inferable_export_typed() {
        // String-literal rhs is statically inferable; no annotation
        // required, no inference error.
        let dir = create_temp_dir("load_inferable_literal");
        fs::write(dir.join("main.crn"), "exports {\n  name = \"carina\"\n}\n").unwrap();

        let loaded = load_configuration_with_config(
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .unwrap();
        cleanup(&dir);
        assert_eq!(
            loaded.parsed.export_params[0].static_type,
            crate::static_type::ExportStaticType::Inferred(crate::static_type::StaticType::Known(
                crate::parser::TypeExpr::String
            )),
        );
    }

    #[test]
    fn load_configuration_directory_with_provider() {
        let dir = create_temp_dir("load_dir_provider");
        fs::write(
            dir.join("test.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_backend() {
        let dir = create_temp_dir("load_dir_backend");
        fs::write(
            dir.join("test.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_nonexistent_path() {
        let path = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_file_xyz");
        let result = load_configuration(&path);
        match result {
            Err(e) => assert!(e.contains("Path not found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for nonexistent path"),
        }
    }

    #[test]
    fn load_configuration_empty_directory() {
        let dir = create_temp_dir("load_empty_dir");
        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(e.contains("No .crn files found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for empty directory"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_single_file() {
        let dir = create_temp_dir("load_dir_single");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_merges_multiple_files() {
        let dir = create_temp_dir("load_dir_merge");
        fs::write(
            dir.join("provider.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("backend.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
        cleanup(&dir);
    }

    fn unavailable_env_name() -> String {
        format!(
            "CARINA_TEST_ISSUE_3974_STRICT_MERGED_MISSING_{}",
            std::process::id()
        )
    }

    #[test]
    fn load_configuration_multi_file_rejects_unavailable_let_binding() {
        let dir = create_temp_dir("unavailable_let_binding");
        let missing = unavailable_env_name();
        unsafe {
            std::env::remove_var(&missing);
        }
        fs::write(
            dir.join("value.crn"),
            format!("let token = env(\"{missing}\")\n"),
        )
        .unwrap();
        fs::write(dir.join("other.crn"), "let marker = \"other\"\n").unwrap();

        let error = match load_configuration(&dir) {
            Ok(_) => panic!("the strict merged pass accepted an unavailable let binding"),
            Err(error) => error,
        };

        assert!(
            error.contains(&format!(
                "env(): environment variable '{missing}' is not set"
            )),
            "unexpected error: {error}",
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_multi_file_rejects_unavailable_backend_attribute() {
        let dir = create_temp_dir("unavailable_backend_attribute");
        let missing = unavailable_env_name();
        unsafe {
            std::env::remove_var(&missing);
        }
        fs::write(
            dir.join("backend.crn"),
            format!(
                r#"backend s3 {{
    bucket = env("{missing}")
    key    = "state.json"
    region = "ap-northeast-1"
}}
"#,
            ),
        )
        .unwrap();
        fs::write(dir.join("other.crn"), "let marker = \"other\"\n").unwrap();

        let error = match load_configuration(&dir) {
            Ok(_) => panic!("the strict merged pass accepted an unavailable backend attribute"),
            Err(error) => error,
        };

        assert!(
            error.contains(&format!(
                "env(): environment variable '{missing}' is not set"
            )),
            "unexpected error: {error}",
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_multi_file_does_not_open_secret_for_strict_evaluation() {
        let dir = create_temp_dir("unavailable_inside_secret");
        let missing = unavailable_env_name();
        unsafe {
            std::env::remove_var(&missing);
        }
        fs::write(
            dir.join("value.crn"),
            format!("let token = secret(env(\"{missing}\"))\n"),
        )
        .unwrap();
        fs::write(dir.join("other.crn"), "let marker = \"other\"\n").unwrap();

        let loaded = load_configuration(&dir)
            .expect("the strict merged pass must not descend into a typed secret");

        assert!(matches!(
            loaded.parsed.variables.get("token"),
            Some(Value::Deferred(DeferredValue::Secret(inner)))
                if matches!(inner.as_ref(), Value::Deferred(DeferredValue::Unavailable { .. }))
        ));
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_ignores_non_crn_files() {
        let dir = create_temp_dir("load_dir_ignore_non_crn");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "not a crn file").unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_parse_error() {
        let dir = create_temp_dir("load_dir_parse_error");
        fs::write(dir.join("bad.crn"), "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&dir);
        assert!(result.is_err());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_renders_a_function_body_file_once() {
        let dir = create_temp_dir("function_body_file_once");
        let main = dir.join("main.crn");
        let functions = dir.join("fns.crn");
        fs::write(
            &main,
            "let vpc = mock.test.resource { name = \"vpc\" }\n\
             mock.test.resource { name = g(vpc.name) }\n",
        )
        .unwrap();
        fs::write(&functions, "fn g(r: String): String { r.name }\n").unwrap();
        fs::write(dir.join("providers.crn"), "provider mock {}\n").unwrap();

        let error = match load_configuration(&dir) {
            Ok(_) => panic!("invalid function body unexpectedly loaded"),
            Err(error) => error,
        };

        assert_eq!(
            error.matches(&functions.display().to_string()).count(),
            1,
            "{error}"
        );
        assert!(!error.contains(&main.display().to_string()), "{error}");
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_renders_a_same_file_function_error_once() {
        let dir = create_temp_dir("same_file_function_error_once");
        let main = dir.join("main.crn");
        fs::write(
            &main,
            "fn g(r: String): String { r.name }\n\
             let vpc = mock.test.resource { name = \"vpc\" }\n\
             mock.test.resource { name = g(vpc.name) }\n",
        )
        .unwrap();
        fs::write(dir.join("providers.crn"), "provider mock {}\n").unwrap();

        let error = match load_configuration(&dir) {
            Ok(_) => panic!("invalid function body unexpectedly loaded"),
            Err(error) => error,
        };

        assert_eq!(
            error.matches(&main.display().to_string()).count(),
            1,
            "{error}"
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_rejects_file_path() {
        let dir = create_temp_dir("load_rejects_file");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_file_path_rejected_even_with_bad_content() {
        let dir = create_temp_dir("load_file_parse_error");
        let file = dir.join("bad.crn");
        fs::write(&file, "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_stamps_full_path_on_warnings() {
        // Issue #1997: warnings and deferred for-expressions must carry a full
        // source path, not a bare filename. Bare filenames are ambiguous when a
        // project spans multiple .crn files and break editor jump-to-location.
        let dir = create_temp_dir("load_warning_full_path");
        // A for-loop with an unused binding generates a ParseWarning.
        let crn_path = dir.join("main.crn");
        fs::write(
            &crn_path,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let empty_list = []
let _ = for unused_var in empty_list {
    aws.ec2.Vpc {
        cidr_block = "10.0.0.0/16"
    }
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(
            !config.parsed.warnings.is_empty(),
            "fixture must produce at least one ParseWarning"
        );
        let stamped = config.parsed.warnings[0]
            .file
            .as_ref()
            .expect("warning must have a file path stamped");
        assert_eq!(
            stamped,
            &crn_path.display().to_string(),
            "warning.file must carry the full source path, not a bare filename",
        );
        cleanup(&dir);
    }

    #[test]
    fn resolved_directory_deferred_loop_removes_its_not_yet_available_warning() {
        let dir = create_temp_dir("resolved_deferred_warning");
        let crn_path = dir.join("main.crn");
        fs::write(
            &crn_path,
            r#"let orgs = upstream_state { source = "../orgs" }

for account_id in orgs.accounts {
    mock.test.resource { name = account_id }
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).expect("directory fixture loads");
        let mut parsed = crate::module_resolver::resolve_modules_with_providerless_config(
            config.parsed,
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .expect("directory fixture crosses the module-resolution seam");
        assert_eq!(parsed.deferred_for_expressions().len(), 1);
        let deferred_line = parsed.deferred_for_expressions()[0].line();
        parsed.warnings.push(crate::parser::ParseWarning {
            file: Some(crn_path.display().to_string()),
            line: deferred_line,
            kind: crate::parser::WarningKind::DeferredFor,
            span: None,
            message: "upstream iterable is not yet available".to_string(),
        });

        let bindings =
            crate::binding_index::IterableBindings::from_upstream_only(HashMap::from([(
                "orgs".to_string(),
                HashMap::from([(
                    "accounts".to_string(),
                    crate::resource::ExpandedValue::Concrete(
                        crate::resource::ExpandedConcreteValue::List(vec![
                            crate::resource::ExpandedValue::Concrete(
                                crate::resource::ExpandedConcreteValue::String(
                                    "123456789012".into(),
                                ),
                            ),
                        ]),
                    ),
                )]),
            )]));
        let mut parsed = parsed
            .into_expanded()
            .expect("directory fixture must contain no parser placeholders");
        parsed
            .expand_deferred_for_expressions(&bindings)
            .expect("resolved upstream iterable expands");

        assert!(parsed.deferred_for_expressions().is_empty());
        assert!(
            parsed
                .warnings
                .iter()
                .all(|warning| warning.kind != crate::parser::WarningKind::DeferredFor),
            "resolved loop retained stale warnings: {:?}",
            parsed.warnings,
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_preserves_unresolved_parsed() {
        let dir = create_temp_dir("load_unresolved");
        fs::write(
            dir.join("test.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        // unresolved_parsed should also have the provider
        assert_eq!(config.unresolved_parsed.parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_multiple_backends_error() {
        let dir = create_temp_dir("load_dir_multi_backend");
        fs::write(
            dir.join("a.crn"),
            r#"backend s3 {
    bucket = "bucket-a"
    key    = "state-a.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("b.crn"),
            r#"backend s3 {
    bucket = "bucket-b"
    key    = "state-b.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(
                e.contains("multiple backend blocks defined"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for multiple backends"),
        }
        cleanup(&dir);
    }

    #[test]
    fn parse_directory_accepts_cross_file_upstream_state_in_for_expression() {
        let dir = create_temp_dir("cross_file_upstream_for");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.Bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        assert!(
            result.is_ok(),
            "expected cross-file upstream_state binding in `for` to resolve, got: {:?}",
            result.err()
        );
    }

    fn deferred_for_body_fixture(main: &str) -> tempfile::TempDir {
        let fixture = tempfile::tempdir().expect("deferred-for fixture");
        fs::create_dir(fixture.path().join("child")).expect("child module directory");
        fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("providers fixture");
        fs::write(
            fixture.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("backend fixture");
        fs::write(
            fixture.path().join("child/main.crn"),
            "arguments { item: String }\nmock.test.resource { name = item }\n",
        )
        .expect("child module fixture");
        fs::write(fixture.path().join("main.crn"), main).expect("main fixture");
        fixture
    }

    fn resolve_deferred_for_fixture(
        fixture: &tempfile::TempDir,
    ) -> Result<crate::parser::ModuleResolvedInferredFile, crate::module_resolver::ModuleError>
    {
        let parsed = load_configuration(fixture.path())
            .expect("the parser-side deferred body must survive directory loading")
            .parsed;
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            fixture.path(),
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
    }

    #[test]
    fn sibling_list_expands_module_call_and_read_for_bodies() {
        let fixture = deferred_for_body_fixture(
            "let child = use { source = './child' }\n\
             let mods = for n in names { child { item = n } }\n\
             let reads = for n in names { read mock.iam.Roles { name_regex = n } }\n",
        );
        fs::write(
            fixture.path().join("values.crn"),
            "let names = ['alpha', 'beta']\n",
        )
        .expect("sibling values fixture");

        let loaded = load_configuration(fixture.path())
            .expect("a sibling plan-time list must expand during the final parse pass");
        let call_bindings = loaded
            .parsed
            .module_calls
            .iter()
            .filter_map(|call| call.binding_name.as_deref())
            .collect::<Vec<_>>();
        let read_bindings = loaded
            .parsed
            .data_sources
            .iter()
            .filter_map(|read| read.binding.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(call_bindings, ["mods[0]", "mods[1]"]);
        assert_eq!(read_bindings, ["reads[0]", "reads[1]"]);
    }

    #[test]
    fn later_list_expands_module_call_and_read_for_bodies() {
        let fixture = deferred_for_body_fixture(
            "let child = use { source = './child' }\n\
             let mods = for n in names { child { item = n } }\n\
             let reads = for n in names { read mock.iam.Roles { name_regex = n } }\n\
             let names = ['alpha', 'beta']\n",
        );

        let loaded = load_configuration(fixture.path())
            .expect("a later plan-time list must expand during the final parse pass");
        assert_eq!(loaded.parsed.module_calls.len(), 2);
        assert_eq!(loaded.parsed.data_sources.len(), 2);
    }

    #[test]
    fn sibling_map_indexed_by_loop_variable_expands_resources() {
        let fixture = deferred_for_body_fixture(
            "let resources = for name in names {\n\
               mock.test.resource { name = lookup[name] }\n\
             }\n",
        );
        fs::write(
            fixture.path().join("values.crn"),
            "let names = ['one', 'two']\nlet lookup = { one = 'A', two = 'B' }\n",
        )
        .expect("sibling lookup fixture");

        let loaded = load_configuration(fixture.path())
            .expect("provisional template errors must be retried in the final parse pass");
        let names = loaded
            .parsed
            .resources
            .iter()
            .map(|resource| match resource.get_attr("name") {
                Some(Value::Concrete(crate::resource::ConcreteValue::String(name))) => {
                    name.expose_plaintext()
                }
                other => panic!("expected concrete resource name, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["A", "B"]);
    }

    #[test]
    fn deferred_for_body_template_error_retains_loop_site_and_error() {
        let fixture = tempfile::tempdir().expect("deferred body error fixture");
        let file = fixture.path().join("main.crn");
        let source = "let src = mock.test.resource { name = 'src' }\n\
                      let lookup = { one = 'A' }\n\
                      let resources = for name in src.list { mock.test.resource { name = lookup[name] } }\n";
        let error =
            parse_directory_files(&[(file, source.to_string())], &ProviderContext::default())
                .expect_err("a dynamic template subscript must fail at the deferred-loop boundary");

        let parser::ParseError::DeferredForBodyTemplate {
            line,
            header,
            error: inner,
            ..
        } = &error
        else {
            panic!("expected a typed deferred-body template error, got {error:?}");
        };
        assert_eq!(*line, 3);
        assert_eq!(header, "for name in src.list");
        assert!(matches!(
            inner.as_ref(),
            parser::ParseError::InvalidExpression { message, .. }
                if message.contains("index access key must be an integer or string")
        ));
        let message = error.to_string();
        assert!(message.contains("for name in src.list"), "{message}");
        assert!(message.contains("line 3"), "{message}");
        assert!(
            message.contains("loop value is not known at plan time"),
            "{message}"
        );
    }

    #[test]
    fn typo_iterable_reports_undefined_identifier() {
        let fixture = deferred_for_body_fixture(
            "let child = use { source = './child' }\n\
             let items = ['alpha', 'beta']\n\
             let mods = for n in itmes { child { item = n } }\n",
        );

        let error = resolve_deferred_for_fixture(&fixture)
            .expect_err("finalization must diagnose an undefined iterable");
        let crate::module_resolver::ModuleError::ParseInFile { path, error } = error.root_cause()
        else {
            panic!("expected a file-owned typed parse error, got {error:?}");
        };
        assert!(
            path.ends_with("main.crn"),
            "unexpected error owner: {path:?}"
        );
        assert!(
            matches!(
                error.as_ref(),
                crate::parser::ParseError::UndefinedDeferredIterable {
                    name,
                    line: 3,
                    suggestion: Some(suggestion),
                    ..
                } if name == "itmes" && suggestion == "items"
            ),
            "expected the typed undefined-iterable diagnostic, got {error:?}"
        );
    }

    #[test]
    fn module_resolution_rejects_runtime_deferred_module_call_and_read_bodies() {
        for (body, expected_body) in [
            (
                "child { item = x }",
                crate::parser::DeferredForBodyKind::ModuleCall {
                    name: "child".to_string(),
                },
            ),
            (
                "read mock.iam.Roles { name_regex = x }",
                crate::parser::DeferredForBodyKind::Read {
                    resource_type: "mock.iam.Roles".to_string(),
                },
            ),
        ] {
            let fixture = deferred_for_body_fixture(&format!(
                "let child = use {{ source = './child' }}\n\
                 let src = mock.test.resource {{ name = 'src' }}\n\
                 let items = for x in src.list {{ {body} }}\n"
            ));

            let error = resolve_deferred_for_fixture(&fixture)
                .expect_err("the final module-resolution seam must reject this deferred body");
            let crate::module_resolver::ModuleError::ParseInFile { path, error } =
                error.root_cause()
            else {
                panic!("expected a file-owned typed parse error, got {error:?}");
            };
            assert!(
                path.ends_with("main.crn"),
                "unexpected error owner: {path:?}"
            );
            assert!(matches!(
                error.as_ref(),
                crate::parser::ParseError::DeferredForBodyUnsupported(details)
                    if details.line == 3
                        && details.header == "for x in src.list"
                        && details.body == expected_body
            ));
            let message = error.to_string();
            assert!(message.contains("for x in src.list"), "{message}");
            assert!(message.contains("line 3"), "{message}");
            assert!(
                message.contains("still unresolved after module expansion"),
                "{message}"
            );
            assert!(message.contains("managed resources directly"), "{message}");
        }
    }

    #[test]
    fn module_argument_iterable_is_substituted_before_body_rejection() {
        let fixture = tempfile::tempdir().expect("module-argument fixture");
        fs::create_dir_all(fixture.path().join("parent/grandchild"))
            .expect("nested module directories");
        fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("providers fixture");
        fs::write(
            fixture.path().join("main.crn"),
            "let parent = use { source = './parent' }\n\
             parent { names = ['alpha', 'beta'] }\n",
        )
        .expect("root fixture");
        fs::write(
            fixture.path().join("parent/main.crn"),
            "arguments { names: list(String) }\n\
             let grandchild = use { source = './grandchild' }\n\
             let calls = for name in names { grandchild { item = name } }\n",
        )
        .expect("parent fixture");
        fs::write(
            fixture.path().join("parent/grandchild/main.crn"),
            "arguments { item: String }\nmock.test.resource { name = item }\n",
        )
        .expect("grandchild fixture");

        let error = resolve_deferred_for_fixture(&fixture)
            .expect_err("module-call loop replay is not supported at the final seam");
        let crate::module_resolver::ModuleError::ParseInFile { path, error } = error.root_cause()
        else {
            panic!("expected a file-owned typed parse error, got {error:?}");
        };
        assert!(
            path.ends_with("parent/main.crn"),
            "unexpected error owner: {path:?}"
        );
        assert!(
            matches!(
                error.as_ref(),
                crate::parser::ParseError::DeferredForBodyUnsupported(details)
                    if details.line == 3
                        && details.header == "for name in names"
                        && matches!(
                            &details.body,
                            crate::parser::DeferredForBodyKind::ModuleCall { name }
                                if name == "grandchild"
                        )
            ),
            "unexpected deferred-body error: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("concrete after module argument substitution"),
            "{message}"
        );
        assert!(!message.contains("not known until apply"), "{message}");
        assert!(!message.contains("still unresolved"), "{message}");
    }

    #[test]
    fn nested_module_argument_iterable_reports_module_argument_classification() {
        let fixture = tempfile::tempdir().expect("nested module-argument fixture");
        fs::create_dir_all(fixture.path().join("a")).expect("module A directory");
        fs::create_dir_all(fixture.path().join("b")).expect("module B directory");
        fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("providers fixture");
        fs::write(
            fixture.path().join("main.crn"),
            "let a = use { source = './a' }\na { names = ['x'] }\n",
        )
        .expect("root fixture");
        fs::write(
            fixture.path().join("a/main.crn"),
            "arguments { names: list(String) }\n\
             let b = use { source = '../b' }\n\
             b { items = names }\n",
        )
        .expect("module A fixture");
        fs::write(
            fixture.path().join("b/main.crn"),
            "arguments { items: list(String) }\n\
             let reads = for i in items { read mock.test.ds { name = i } }\n",
        )
        .expect("module B fixture");

        let error = resolve_deferred_for_fixture(&fixture)
            .expect_err("nested deferred read replay must be rejected");
        let crate::module_resolver::ModuleError::ParseInFile { path, error } = error.root_cause()
        else {
            panic!("expected a file-owned typed parse error, got {error:?}");
        };
        assert!(path.ends_with("b/main.crn"), "unexpected owner: {path:?}");
        let crate::parser::ParseError::DeferredForBodyUnsupported(details) = error.as_ref() else {
            panic!("expected a deferred-body error, got {error:?}");
        };
        assert_eq!(details.header, "for i in items");
        assert_eq!(
            details.body,
            crate::parser::DeferredForBodyKind::Read {
                resource_type: "mock.test.ds".to_string(),
            }
        );
        assert_eq!(
            details.iterable_resolution,
            crate::parser::DeferredForIterableResolution::DependsOnEnclosingModuleArgument,
            "an enclosing module-argument placeholder needs its own classification",
        );
        let message = error.to_string();
        assert!(message.contains("module argument"), "{message}");
        assert!(message.contains("Move the loop to the caller"), "{message}");
    }

    #[test]
    fn directory_validate_path_keeps_deferred_for_resource_placeholder() {
        let fixture = deferred_for_body_fixture(
            "let child = use { source = './child' }\n\
             let src = mock.test.resource { name = 'src' }\n\
             let res_items = for x in src.list {\n\
               mock.test.resource { name = x comment = secret('fixed') }\n\
             }\n",
        );

        let parsed = resolve_deferred_for_fixture(&fixture)
            .expect("a managed-resource deferred-for body remains supported");
        let [deferred] = parsed.deferred_for_expressions().as_slice() else {
            panic!(
                "expected exactly one deferred placeholder, got {:?}",
                parsed.deferred_for_expressions()
            );
        };
        assert_eq!(deferred.header, "for x in src.list");
        assert_eq!(deferred.line(), 3);
        assert_eq!(deferred.resource_type(), "mock.test.resource");
        assert_eq!(deferred.template_resource_binding(), Some("res_items[?]"));
        assert!(matches!(
            deferred.template_resource().attributes().get("comment"),
            Some(crate::resource::Value::Deferred(
                crate::resource::DeferredValue::Secret(_)
            ))
        ));
    }

    #[test]
    fn cross_file_upstream_state_refs_emit_no_soft_warning() {
        // Field validity is checked statically by the `upstream_exports`
        // module now. Loader- and parser-level "validate does not inspect"
        // soft warnings are gone across the board.
        let dir = create_temp_dir("cross_file_upstream_no_warning");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}

let network = upstream_state {
  source = '../network'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.Bucket {
    name = name
  }
}

awscc.ec2.SecurityGroup {
  group_description = 'Web SG'
  vpc_id = network.vpc_id
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        let loaded = result.expect("load should succeed");
        cleanup(&dir);

        let upstream_warnings: Vec<_> = loaded
            .parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("upstream_state"))
            .collect();
        assert!(
            upstream_warnings.is_empty(),
            "loader should emit no soft upstream_state warnings, got: {:?}",
            upstream_warnings
        );
    }

    #[test]
    fn parse_directory_leaves_undefined_iterable_reference_for_scope_check() {
        let dir = create_temp_dir("undefined_for_iterable");
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  aws.s3.Bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        let parsed = result.expect("parse_directory should not fail on undefined iterable");
        let errs = parser::check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        match &errs[0] {
            parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn provisional_sibling_if_rejects_mixed_branches_after_classification() {
        let temp = tempfile::tempdir().unwrap();
        let files = vec![
            (temp.path().join("a.crn"), "let flag = false\n".to_string()),
            (
                temp.path().join("b.crn"),
                "let selected = if flag { mock.test.Thing { name = \"unused\" } } else { \"fallback\" }\n"
                    .to_string(),
            ),
            (
                temp.path().join("c.crn"),
                "let observed = selected\n".to_string(),
            ),
        ];

        let error = parse_directory_files(&files, &ProviderContext::default())
            .expect_err("the final seeded pass must reject mixed structural/value branches");
        assert!(
            error.to_string().contains(
                "both branches of this `if` must be the same kind: the `then` branch declares resources/module calls but the `else` branch produces a value; make both branches values, or both branches resource/module declarations"
            ),
            "unexpected fixed-point error: {error}",
        );
    }

    #[test]
    fn diagnostic_semantic_file_resolves_function_argument_from_sibling_let() {
        let temp = tempfile::tempdir().unwrap();
        let main_path = temp.path().join("main.crn");
        fs::write(
            &main_path,
            "let target = test.test.resource {\n  mode = upper(sibling_mode)\n}\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("values.crn"),
            "provider test {}\nlet sibling_mode = \"invalid\"\n",
        )
        .unwrap();

        let directory = parse_directory_with_overrides_and_diagnostics(
            temp.path(),
            &ProviderContext::default(),
            &HashMap::new(),
        )
        .expect("directory parse");
        let semantic = directory.semantic_file(&main_path).expect("semantic file");
        let mode = semantic
            .iter_all_resources()
            .find(|resource| resource.binding() == Some("target"))
            .and_then(|resource| resource.attributes().get("mode").cloned())
            .expect("target.mode");

        assert_eq!(
            mode,
            Value::Concrete(crate::resource::ConcreteValue::String("INVALID".into()))
        );
    }

    #[test]
    fn shared_directory_parse_assigns_diagnostic_sources_during_parsing() {
        let temp = tempfile::tempdir().unwrap();
        let attributes_path = temp.path().join("attributes.crn");
        let exports_path = temp.path().join("exports.crn");
        fs::write(
            &attributes_path,
            "let child = use { source = './child' }\n\
             arguments {\n\
               input: String {\n\
                 validation { condition = input != 'invalid' }\n\
               }\n\
             }\n\
             require input != 'invalid', \"input is required\"\n\
             child {}\n\
             attributes {\n    output: String = 'value'\n}\n",
        )
        .unwrap();
        fs::write(
            &exports_path,
            "exports {\n    result: String = 'value'\n}\n",
        )
        .unwrap();

        let files = vec![
            (
                attributes_path.clone(),
                fs::read_to_string(&attributes_path).unwrap(),
            ),
            (
                exports_path.clone(),
                fs::read_to_string(&exports_path).unwrap(),
            ),
        ];
        let parsed =
            parse_directory_files(&files, &ProviderContext::default()).expect("directory parse");
        let attribute_file = parsed[0].1.parsed();
        let attribute = &attribute_file.attribute_params[0];
        assert_eq!(attribute.source_file(), attributes_path.as_path());
        assert_eq!(attribute.source_span().start_line(), 10);
        assert_eq!(
            attribute_file.module_calls[0].source_file(),
            attributes_path.as_path()
        );
        assert_eq!(
            attribute_file.arguments[0].validations[0].source_file(),
            attributes_path.as_path()
        );
        assert_eq!(
            attribute_file.requires[0].source_file(),
            attributes_path.as_path()
        );
        let export_file = parsed[1].1.parsed();
        let export = &export_file.export_params[0];
        assert_eq!(export.source_file(), exports_path.as_path());
        assert_eq!(export.source_span().start_line(), 2);
    }

    fn duplicate_messages(dir: &Path, sources: &[(&str, &str)]) -> Vec<String> {
        let files: Vec<_> = sources
            .iter()
            .map(|(name, source)| (dir.join(name), (*source).to_string()))
            .collect();
        let parsed_files =
            parse_directory_files(&files, &ProviderContext::default()).expect("parse directory");
        find_duplicate_declarations(dir, &parsed_files)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn duplicate_messages_for_two_copies(parsed: ParsedFile) -> Vec<String> {
        let temp = tempfile::tempdir().unwrap();
        let parsed_files: Vec<_> = ["a.crn", "b.crn"]
            .into_iter()
            .map(|name| {
                let parsed = parsed.clone();
                let local_variable_names = parsed.variables.keys().cloned().collect();
                (
                    temp.path().join(name),
                    DirectoryResolvedFile {
                        parsed,
                        local_variable_names,
                    },
                )
            })
            .collect();

        find_duplicate_declarations(temp.path(), &parsed_files)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn duplicate_extraction_deduplicates_resource_variable_echo() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[(
                "main.crn",
                "let echoed = mock.test.Thing {\n  name = \"echoed\"\n}\n",
            )],
        );

        assert!(messages.is_empty(), "unexpected duplicates: {messages:#?}");
    }

    #[test]
    fn duplicate_extraction_excludes_discard_binding() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[("a.crn", "let _ = \"a\"\n"), ("b.crn", "let _ = \"b\"\n")],
        );

        assert!(messages.is_empty(), "unexpected duplicates: {messages:#?}");
    }

    #[test]
    fn duplicate_extraction_only_treats_underscore_as_discard_in_binding_namespace() {
        let temp = tempfile::tempdir().unwrap();
        let parsed_files: Vec<_> = ["a.crn", "b.crn"]
            .into_iter()
            .map(|name| {
                let parsed = ParsedFile {
                    export_params: vec![crate::parser::ParsedExportParam::dummy(
                        "_".to_string(),
                        None,
                        None,
                    )],
                    ..ParsedFile::default()
                };
                (
                    temp.path().join(name),
                    DirectoryResolvedFile {
                        parsed,
                        local_variable_names: HashSet::new(),
                    },
                )
            })
            .collect();
        let messages: Vec<_> = find_duplicate_declarations(temp.path(), &parsed_files)
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(
            messages,
            vec!["duplicate export '_': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_counts_value_lets_by_source_file() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[("a.crn", "let v = \"a\"\n"), ("b.crn", "let v = \"b\"\n")],
        );

        assert_eq!(
            messages,
            vec!["duplicate binding 'v': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_covers_resource_bindings() {
        let mut resource = crate::resource::AuthoredResource::new("mock.test.Thing", "shared");
        resource.binding = Some(crate::parser::RelativeBindingName::new("shared"));
        let parsed = ParsedFile {
            resources: vec![resource],
            ..ParsedFile::default()
        };

        assert_eq!(
            duplicate_messages_for_two_copies(parsed),
            vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_covers_data_source_bindings() {
        let mut data_source = crate::resource::AuthoredDataSource::new("mock.test.Thing", "shared");
        data_source.binding = Some(crate::parser::RelativeBindingName::new("shared"));
        let parsed = ParsedFile {
            data_sources: vec![data_source],
            ..ParsedFile::default()
        };

        assert_eq!(
            duplicate_messages_for_two_copies(parsed),
            vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_covers_module_call_bindings() {
        let parsed = ParsedFile {
            module_calls: vec![ModuleCall::test(
                "module".to_string(),
                Some("shared".to_string()),
                HashMap::new(),
            )],
            ..ParsedFile::default()
        };

        assert_eq!(
            duplicate_messages_for_two_copies(parsed),
            vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_covers_wait_bindings() {
        let parsed = ParsedFile {
            wait_bindings: vec![crate::parser::WaitBinding {
                binding: crate::parser::RelativeBindingName::from("shared"),
                target: crate::parser::RelativeBindingName::from("target"),
                until_raw: "target.status == 'ready'".to_string(),
                until_predicate: crate::parser::UntilPredicateAst {
                    root: crate::parser::RelativeBindingName::new("target"),
                    attribute_segments: vec!["status".to_string()],
                    rhs: Value::Concrete(crate::resource::ConcreteValue::String("ready".into())),
                },
                timeout_secs: None,
                depends_on: Vec::new(),
                line: 1,
            }],
            ..ParsedFile::default()
        };

        assert_eq!(
            duplicate_messages_for_two_copies(parsed),
            vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_covers_named_provider_bindings() {
        let parsed = ParsedFile {
            providers: vec![crate::parser::ProviderConfig {
                name: "mock".to_string(),
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: Some("shared".to_string()),
            }],
            ..ParsedFile::default()
        };

        assert_eq!(
            duplicate_messages_for_two_copies(parsed),
            vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_rejects_default_provider_kind_across_files() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[
                (
                    "providers.crn",
                    "provider mock {\n  source = 'file:///mock.wasm'\n}\n",
                ),
                ("main.crn", "provider mock {}\n"),
            ],
        );

        assert_eq!(
            messages,
            vec!["duplicate provider kind 'mock': declared in main.crn and providers.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_allows_named_instances_of_the_same_provider_kind() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[
                ("providers.crn", "provider mock {}\n"),
                (
                    "main.crn",
                    "let first = provider mock {}\nlet second = provider mock {}\n",
                ),
            ],
        );

        assert!(messages.is_empty(), "unexpected duplicates: {messages:#?}");
    }

    #[test]
    fn duplicate_extraction_groups_cross_file_arguments_with_bindings() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[
                ("a.crn", "arguments {\n  a: String = \"x\"\n}\n"),
                ("b.crn", "let a = \"y\"\n"),
            ],
        );

        assert_eq!(
            messages,
            vec!["duplicate name 'a': declared as argument in a.crn and as binding in b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_keeps_cross_file_argument_message() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[
                ("a.crn", "arguments {\n  a: String = \"x\"\n}\n"),
                ("b.crn", "arguments {\n  a: String = \"y\"\n}\n"),
            ],
        );

        assert_eq!(
            messages,
            vec!["duplicate argument 'a': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn parser_still_rejects_argument_and_binding_in_one_file() {
        let temp = tempfile::tempdir().unwrap();
        let files = vec![(
            temp.path().join("main.crn"),
            "arguments {\n  a: String = \"x\"\n}\nlet a = \"y\"\n".to_string(),
        )];

        let error = parse_directory_files(&files, &ProviderContext::default())
            .expect_err("an argument and let binding share the parser namespace");
        assert!(
            matches!(error, parser::ParseError::DuplicateBinding { ref name, .. } if name == "a"),
            "expected DuplicateBinding for `a`, got: {error:?}",
        );
    }

    #[test]
    fn module_attributes_and_bindings_remain_separate_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let same_file = duplicate_messages(
            temp.path(),
            &[(
                "same.crn",
                "attributes {\n  shared = \"attribute\"\n}\nlet shared = \"binding\"\n",
            )],
        );
        let cross_file = duplicate_messages(
            temp.path(),
            &[
                ("a.crn", "attributes {\n  shared = \"attribute\"\n}\n"),
                ("b.crn", "let shared = \"binding\"\n"),
            ],
        );

        assert!(
            same_file.is_empty(),
            "the parser allows attribute and binding names to overlap: {same_file:#?}",
        );
        assert!(
            cross_file.is_empty(),
            "directory validation must mirror the parser namespace: {cross_file:#?}",
        );
    }

    #[test]
    fn user_functions_and_bindings_remain_separate_namespaces() {
        let temp = tempfile::tempdir().unwrap();
        let same_file = duplicate_messages(
            temp.path(),
            &[(
                "same.crn",
                "fn f() {\n  \"function\"\n}\nlet f = \"binding\"\n",
            )],
        );
        let cross_kind = duplicate_messages(
            temp.path(),
            &[
                ("a.crn", "fn f() {\n  \"function\"\n}\n"),
                ("b.crn", "let f = \"binding\"\n"),
            ],
        );
        let duplicate_functions = duplicate_messages(
            temp.path(),
            &[
                ("a.crn", "fn f() {\n  \"first\"\n}\n"),
                ("b.crn", "fn f() {\n  \"second\"\n}\n"),
            ],
        );

        assert!(
            same_file.is_empty(),
            "the parser allows function and binding names to overlap: {same_file:#?}",
        );
        assert!(
            cross_kind.is_empty(),
            "directory validation must keep functions and bindings separate: {cross_kind:#?}",
        );
        assert_eq!(
            duplicate_functions,
            vec!["duplicate function 'f': declared in a.crn and b.crn"]
        );
    }

    #[test]
    fn duplicate_extraction_uses_multiset_semantics_for_exports() {
        let temp = tempfile::tempdir().unwrap();
        let messages = duplicate_messages(
            temp.path(),
            &[("main.crn", "exports {\n  x = \"one\"\n  x = \"two\"\n}\n")],
        );

        assert_eq!(
            messages,
            vec!["duplicate export 'x': declared 2 times in main.crn"]
        );
    }

    #[test]
    fn located_parse_failure_renders_its_body_file_once() {
        let caller = Path::new("q3/main.crn");
        let body = Path::new("q3/fns.crn");
        let body_error = parser::ParseError::InvalidExpression {
            line: 1,
            message: "cannot apply access .name to string".to_string(),
        }
        .in_file(body);
        let located = LocatedParseError::in_file(caller, body_error);
        let location = located
            .source_file
            .clone()
            .unwrap_or_else(|| caller.to_path_buf());
        let failure = DirectoryParseFailure::Parse {
            location,
            source_file: located.source_file,
            error: located.error,
        };

        assert_eq!(
            failure.to_string(),
            "q3/fns.crn: Invalid expression at line 1: cannot apply access .name to string",
        );
    }

    #[test]
    fn located_parse_failure_does_not_repeat_the_same_file() {
        let file = Path::new("p4/main.crn");
        let error = parser::ParseError::InvalidExpression {
            line: 2,
            message: "bad call".to_string(),
        }
        .in_file(file);
        let located = LocatedParseError::in_file(file, error);
        let failure = DirectoryParseFailure::Parse {
            location: located.source_file.clone().unwrap(),
            source_file: located.source_file,
            error: located.error,
        };

        assert_eq!(
            failure.to_string(),
            "p4/main.crn: Invalid expression at line 2: bad call",
        );
    }
}
