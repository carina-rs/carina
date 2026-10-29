//! `ModuleResolver` driver: import processing, nested-module resolution,
//! and the top-level `resolve_modules*` entry points.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::parser::{
    ModuleCallDiagnosticSource, ModuleCallId, ParseError, ParsedFile, ProviderContext, UseStatement,
};
use crate::resource::ModuleConstraintId;
use crate::schema::SchemaRegistry;

use super::error::ModuleError;
use super::expander::instance_prefix_for_call;
use super::loader::sorted_crn_paths_in;
use super::validation::ModuleConstraintDiagnostic;

/// Resolver-owned result of evaluating one authored module constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModuleConstraintOutcome {
    pub id: ModuleConstraintId,
    pub status: ResolvedModuleConstraintStatus,
}

/// Value-time state observed by the resolver after its fixed-point argument
/// substitution has completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedModuleConstraintStatus {
    Satisfied,
    Pending,
    Failed {
        diagnostic: Box<ModuleConstraintDiagnostic>,
        origin: ResolvedModuleConstraintOrigin,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedModuleConstraintOrigin {
    Argument(crate::parser::ArgumentOrigin),
    Call,
}

/// Constraint outcomes and authored location for one expanded module call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleCallConstraintReport {
    pub source: ModuleCallDiagnosticSource,
    pub outcomes: Vec<ResolvedModuleConstraintOutcome>,
}

impl ModuleCallConstraintReport {
    pub(crate) fn new(
        source: ModuleCallDiagnosticSource,
        outcomes: Vec<ResolvedModuleConstraintOutcome>,
    ) -> (ModuleCallId, Self) {
        (source.id.clone(), Self { source, outcomes })
    }
}

pub type ModuleCallConstraintReports = IndexMap<ModuleCallId, ModuleCallConstraintReport>;

/// Module resolution result for editor diagnostics.
///
/// Normal CLI resolution uses [`resolve_modules_with_config`] and never
/// allocates or clones `constraint_reports`.
pub struct ModuleResolutionDiagnosticReport {
    pub constraint_reports: ModuleCallConstraintReports,
    pub error: Option<ModuleError>,
}

/// Opaque proof that every reachable module definition has passed the shared
/// BindingIndex reference-type validation before expansion.
///
/// Expansion is exposed only as [`Self::expand`]; the resolver and the witness
/// consumed by its internal expansion primitive are private. Consequently a
/// command may skip provider/resource validation, but cannot construct an
/// expansion path that skips reference-type validation.
///
/// ```compile_fail
/// use carina_core::module_resolver::ModuleResolver;
/// // ModuleResolver is private; callers must obtain RefCheckedModuleGraph.
/// let _ = ModuleResolver::new(".");
/// ```
///
/// A checked graph owns exclusive access to the exact parse it checked, so a
/// caller cannot swap in a different parse at expansion time:
///
/// ```compile_fail
/// use std::path::Path;
/// use carina_core::module_resolver::RefCheckedModuleGraph;
/// use carina_core::parser::{ParsedFile, ProviderContext};
/// use carina_core::schema::SchemaRegistry;
///
/// let mut first = ParsedFile::default();
/// let mut second = ParsedFile::default();
/// let config = ProviderContext::default();
/// let schemas = SchemaRegistry::new();
/// let checked = RefCheckedModuleGraph::check(
///     &mut first,
///     Path::new("."),
///     &config,
///     &schemas,
/// ).unwrap();
/// checked.expand(&mut second).unwrap();
/// ```
pub struct RefCheckedModuleGraph<'parsed, 'cfg, E> {
    parsed: &'parsed mut crate::parser::File<E>,
    resolver: ModuleResolver<'cfg, StrictTypeResolution>,
    root_witness: RefCheckWitness,
}

/// Upstream expansion has a deliberately weaker type-name-resolution policy:
/// the consumer need not load every provider used by the upstream project.
/// Keeping that policy in a distinct, private graph prevents ordinary module
/// expansion from opting into it accidentally.
struct UpstreamRefCheckedModuleGraph<'parsed, 'cfg, E> {
    parsed: &'parsed mut crate::parser::File<E>,
    resolver: ModuleResolver<'cfg, DeferredUpstreamTypeResolution>,
    root_witness: RefCheckWitness,
}

/// Private capability produced only by `ModuleResolver::validate_reference_types`.
pub(super) struct RefCheckWitness(pub(super) ());

/// Compile-time policy for resolving type annotations in loaded modules.
pub(crate) trait TypeResolutionPolicy {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError>;
}

/// Normal module resolution requires every declared type to resolve.
pub(crate) struct StrictTypeResolution;

impl TypeResolutionPolicy for StrictTypeResolution {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError> {
        let type_errors = crate::validation::resolve_file_type_exprs(parsed, config);
        if let Some(first) = type_errors.into_iter().next() {
            return Err(ModuleError::Parse(ParseError::InvalidExpression {
                line: 0,
                message: first,
            }));
        }
        Ok(())
    }
}

/// Upstream projects are interpreted with the consumer's incomplete provider
/// registry, so unknown provider-owned types carry no usable constraint.
struct DeferredUpstreamTypeResolution;

impl TypeResolutionPolicy for DeferredUpstreamTypeResolution {
    fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        config: &ProviderContext,
    ) -> Result<(), ModuleError> {
        crate::validation::resolve_file_static_types_for_consumer(parsed, config);
        Ok(())
    }
}

/// Context for module resolution
pub(crate) struct ModuleResolver<'cfg, P = StrictTypeResolution> {
    /// Base directory for resolving relative imports
    pub(super) base_dir: PathBuf,
    /// Cache of loaded modules: path -> ParsedFile
    pub(super) module_cache: HashMap<PathBuf, ParsedFile>,
    /// Currently resolving modules (for cycle detection)
    pub(super) resolving: HashSet<PathBuf>,
    /// Imported module definitions by alias
    pub(super) imported_modules: HashMap<String, ParsedFile>,
    /// `use { source = "..." }` paths by alias — the DSL-visible
    /// source location for each imported module. Threaded into the
    /// `ExpansionTrace` so plan rendering can label a composition
    /// group with `module "<binding>" (<source_path>)` (carina#3322).
    pub(super) module_paths: HashMap<String, String>,
    /// Parser configuration (decryptor, custom validators)
    pub(super) config: &'cfg ProviderContext,
    /// Schema authority used by the mandatory pre-expansion reference check.
    pub(super) schemas: &'cfg SchemaRegistry,
    /// Present only for the editor-facing resolution entry point. Expansion
    /// records into it at the same seam that evaluates fixed-point arguments.
    pub(super) constraint_reports: RefCell<Option<ModuleCallConstraintReports>>,
    /// The type-resolution mode is selected by the resolver's type. This field
    /// is private so callers cannot construct a resolver with a new policy.
    type_resolution: PhantomData<P>,
}

impl<'cfg> ModuleResolver<'cfg, StrictTypeResolution> {
    /// Create a new resolver with the given base directory and default config
    #[cfg(test)]
    pub(crate) fn new(base_dir: impl AsRef<Path>) -> Self {
        static DEFAULT_CONFIG: std::sync::LazyLock<ProviderContext> =
            std::sync::LazyLock::new(ProviderContext::default);
        static EMPTY_SCHEMAS: std::sync::LazyLock<SchemaRegistry> =
            std::sync::LazyLock::new(SchemaRegistry::new);
        Self::with_config_and_schemas(base_dir, &DEFAULT_CONFIG, &EMPTY_SCHEMAS)
    }

    /// Create a new resolver with the given base directory and parser configuration
    pub(crate) fn with_config_and_schemas(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(base_dir, config, schemas)
    }

    #[cfg(test)]
    pub(crate) fn with_config(base_dir: impl AsRef<Path>, config: &'cfg ProviderContext) -> Self {
        static EMPTY_SCHEMAS: std::sync::LazyLock<SchemaRegistry> =
            std::sync::LazyLock::new(SchemaRegistry::new);
        Self::with_config_and_schemas(base_dir, config, &EMPTY_SCHEMAS)
    }

    fn with_constraint_reports(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        let resolver = Self::with_config_and_schemas(base_dir, config, schemas);
        *resolver.constraint_reports.borrow_mut() = Some(IndexMap::new());
        resolver
    }
}

impl<'cfg> ModuleResolver<'cfg, DeferredUpstreamTypeResolution> {
    fn for_upstream(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self::with_type_resolution(base_dir, config, schemas)
    }
}

impl<'cfg, P: TypeResolutionPolicy> ModuleResolver<'cfg, P> {
    fn with_type_resolution(
        base_dir: impl AsRef<Path>,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            module_cache: HashMap::new(),
            resolving: HashSet::new(),
            imported_modules: HashMap::new(),
            module_paths: HashMap::new(),
            config,
            schemas,
            constraint_reports: RefCell::new(None),
            type_resolution: PhantomData,
        }
    }

    pub(super) fn record_constraint_report(
        &self,
        identity: ModuleCallId,
        report: ModuleCallConstraintReport,
    ) {
        if let Some(reports) = self.constraint_reports.borrow_mut().as_mut() {
            reports.insert(identity, report);
        }
    }

    pub(super) fn records_constraint_reports(&self) -> bool {
        self.constraint_reports.borrow().is_some()
    }

    /// Produce the private witness required by every expansion call.
    ///
    /// This is intentionally the only constructor for `RefCheckWitness`.
    /// Resource/provider validation may be skipped by a command, but the
    /// BindingIndex reference relation at module and function boundaries may
    /// not be skipped.
    fn validate_reference_types<E: crate::parser::ExportParamLike>(
        &self,
        parsed: &crate::parser::File<E>,
    ) -> Result<RefCheckWitness, ModuleError> {
        let imported_modules = self.resolved_module_signatures();
        let bindings = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
            parsed,
            self.schemas,
            &imported_modules,
        );
        crate::validation::validate_expansion_boundary_ref_types_with_bindings(
            parsed,
            &imported_modules,
            &bindings,
        )
        .map(|()| RefCheckWitness(()))
        .map_err(ModuleError::ReferenceType)
    }

    fn resolved_module_signatures(&self) -> super::ResolvedModuleSignatures {
        self.imported_modules
            .iter()
            .map(|(alias, module)| {
                (
                    alias.clone(),
                    super::signature::resolved_signature(module, self.config),
                )
            })
            .collect()
    }

    /// Load and cache a module from a directory path.
    ///
    /// Modules are directory-scoped: a module is a directory containing one or
    /// more `.crn` files. Single-file modules are not supported — pass the
    /// module's directory, not an individual `.crn` file.
    pub fn load_module(&mut self, path: &str) -> Result<ParsedFile, ModuleError> {
        let full_path = self.resolve_path(path);

        // Distinguish "file exists but isn't a directory" from
        // "path doesn't exist / permission denied": only the former is the
        // single-file-module contract violation. Otherwise let canonicalize()
        // surface the underlying IO error (NotFound, PermissionDenied, ...).
        let metadata = full_path.metadata()?;
        if !metadata.is_dir() {
            return Err(ModuleError::NotADirectory {
                path: path.to_string(),
            });
        }

        let canonical = full_path.canonicalize()?;

        // Check for circular import
        if self.resolving.contains(&canonical) {
            return Err(ModuleError::CircularImport(path.to_string()));
        }

        // Check cache
        if let Some(module) = self.module_cache.get(&canonical) {
            return Ok(module.clone());
        }

        // Mark as resolving
        self.resolving.insert(canonical.clone());

        let mut parsed = match self.load_directory_module(&full_path) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.resolving.remove(&canonical);
                return Err(e);
            }
        };

        // Verify it's a module (has arguments or attributes)
        if parsed.arguments.is_empty() && parsed.attribute_params.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::NotFound(path.to_string()));
        }

        // Expansion-phase backstops for root-owned blocks inside modules. The
        // CLI's validation walks are the user-facing gates: they collect
        // module-path-prefixed diagnostics and stop before expansion, so
        // validate/plan/apply and lint cannot emit both forms. The production
        // paths that bypass those walks but still expand are destroy, state
        // refresh, and apply's backend-bootstrap re-resolve
        // (`skip_resource_validation = true`). `fixture_plan` also depends on
        // these backstops as test support.
        if !parsed.providers.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ProviderInModule);
        }
        if !parsed.state_blocks.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::StateBlockInModule);
        }
        if parsed.backend.is_some() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::BackendInModule);
        }
        if !parsed.upstream_states.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::UpstreamStateInModule);
        }
        if !parsed.export_params.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ExportsInModule);
        }

        // Recursively resolve nested module imports within this module.
        // The module's directory is used for resolving its relative imports.
        if let Err(e) = self.resolve_nested_modules(&mut parsed, &full_path) {
            self.resolving.remove(&canonical);
            return Err(e);
        }

        // Remove from resolving set
        self.resolving.remove(&canonical);

        // Cache the module
        self.module_cache.insert(canonical, parsed.clone());

        Ok(parsed)
    }

    /// Load all .crn files from a directory and merge them into a single ParsedFile.
    ///
    /// Routed through [`crate::config_loader::parse_directory_files`] so
    /// every file's `ParseContext` is seeded with the binding-name union
    /// from sibling `.crn` files. Without this, `arguments {}` declared
    /// in `main.crn` would be invisible to `${env}` interpolations in
    /// sibling `role.crn` (#2815). #2817.
    fn load_directory_module(&self, dir_path: &Path) -> Result<ParsedFile, ModuleError> {
        let paths = sorted_crn_paths_in(dir_path)?;
        let mut files: Vec<(std::path::PathBuf, String)> = Vec::with_capacity(paths.len());
        for path in paths {
            let content = std::fs::read_to_string(&path)?;
            files.push((path, content));
        }
        let parsed_files = crate::config_loader::parse_directory_files_located(&files, self.config)
            .map_err(|located| match located.source_file {
                Some(path) => ModuleError::ParseInFile {
                    path: crate::utils::normalize_source_path(&path),
                    error: located.error,
                },
                None => ModuleError::Parse(*located.error),
            })?;

        let mut merged = ParsedFile::default();
        for (_, parsed) in parsed_files {
            crate::config_loader::merge_parsed_file(&mut merged, parsed.into_inner());
        }

        // Resolve references while the module's own binding namespace is still
        // intact. Besides folding module-local values, this records the direct
        // dependency bindings that expansion subsequently instance-prefixes.
        // Argument bindings remain deferred for call-site substitution.
        crate::parser::resolve_resource_refs_with_config(&mut merged, self.config)
            .map_err(ModuleError::Parse)?;

        P::resolve_file_type_exprs(&mut merged, self.config)?;
        Ok(merged)
    }

    /// Resolve a relative path to an absolute path
    fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        }
    }

    /// Process imports and store imported modules
    pub fn process_imports(
        &mut self,
        imports: &[UseStatement],
        module_calls: &[crate::parser::ModuleCall],
    ) -> Result<(), ModuleError> {
        for import in imports {
            let module = self.load_module(&import.path).map_err(|error| {
                let owning_calls = module_calls
                    .iter()
                    .filter(|call| call.module_name == import.alias)
                    .map(|call| call.id().clone())
                    .collect();
                error.with_owning_calls(owning_calls)
            })?;
            self.imported_modules.insert(import.alias.clone(), module);
            self.module_paths
                .insert(import.alias.clone(), import.path.clone());
        }
        Ok(())
    }

    /// Resolve nested module imports within a parsed module.
    ///
    /// This processes the module's own imports and module_calls, expanding any
    /// nested modules recursively. Cycle detection is handled by the `resolving`
    /// set in `load_module()`.
    fn resolve_nested_modules(
        &mut self,
        parsed: &mut ParsedFile,
        base_dir: &Path,
    ) -> Result<(), ModuleError> {
        // Save and temporarily replace the base_dir and imported_modules
        let original_base_dir = std::mem::replace(&mut self.base_dir, base_dir.to_path_buf());
        let original_imported = std::mem::take(&mut self.imported_modules);
        let original_paths = std::mem::take(&mut self.module_paths);

        // Process the module's own imports
        let imports = parsed.uses.clone();
        let result = self.process_imports(&imports, &parsed.module_calls);

        if let Err(e) = result {
            // Restore state on error
            self.base_dir = original_base_dir;
            self.imported_modules = original_imported;
            self.module_paths = original_paths;
            return Err(e);
        }

        let ref_check = match self.validate_reference_types(parsed) {
            Ok(witness) => witness,
            Err(error) => {
                self.base_dir = original_base_dir;
                self.imported_modules = original_imported;
                self.module_paths = original_paths;
                return Err(error);
            }
        };

        // Expand the module's own module_calls. `module_calls` is cloned
        // because the loop body now folds the contribution in via
        // `merge_parsed_file(parsed, …)` — a
        // whole-`&mut parsed` borrow that conflicts with any live
        // immutable borrow of a `parsed` field (the old per-field
        // `.extend()` only needed a disjoint field borrow; the single
        // merge surface needs the whole struct).
        let module_calls = parsed.module_calls.clone();
        let signatures = self.resolved_module_signatures();
        let binding_index = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
            parsed,
            self.schemas,
            &signatures,
        );
        let resolved_bindings = crate::binding_index::ResolvedBindings::for_validation(parsed);
        let mut expanded_calls = Vec::with_capacity(module_calls.len());
        for call in &module_calls {
            let instance_prefix = instance_prefix_for_call(call);
            match self.expand_module_call_checked(
                call,
                &instance_prefix,
                &ref_check,
                Some(&binding_index),
                Some(&resolved_bindings),
            ) {
                Ok(expanded) => expanded_calls.push(expanded),
                Err(e) => {
                    self.base_dir = original_base_dir;
                    self.imported_modules = original_imported;
                    self.module_paths = original_paths;
                    return Err(e);
                }
            }
        }
        drop(binding_index);
        for expanded in expanded_calls {
            // The module contribution is a `ParsedFile`; fold it in via the
            // one shared merge surface so no field can be silently dropped.
            crate::config_loader::merge_parsed_file(parsed, expanded);
        }

        // Restore original state
        self.base_dir = original_base_dir;
        self.imported_modules = original_imported;
        self.module_paths = original_paths;

        Ok(())
    }
}

impl<'parsed, 'cfg, E: crate::parser::ExportParamLike> RefCheckedModuleGraph<'parsed, 'cfg, E> {
    /// Load the complete reachable module graph and validate every reference
    /// boundary before making expansion available.
    pub fn check(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let resolver = ModuleResolver::with_config_and_schemas(base_dir, config, schemas);
        Self::check_with_resolver(parsed, resolver)
    }

    fn check_with_reporting(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let resolver = ModuleResolver::with_constraint_reports(base_dir, config, schemas);
        Self::check_with_resolver(parsed, resolver)
    }

    fn check_with_resolver(
        parsed: &'parsed mut crate::parser::File<E>,
        mut resolver: ModuleResolver<'cfg, StrictTypeResolution>,
    ) -> Result<Self, ModuleError> {
        resolver.process_imports(&parsed.uses, &parsed.module_calls)?;
        let root_witness = resolver.validate_reference_types(parsed)?;
        Ok(Self {
            parsed,
            resolver,
            root_witness,
        })
    }

    /// Consume the checked graph and expand its root configuration.
    pub fn expand(mut self) -> Result<(), ModuleError> {
        resolve_modules_with_resolver(self.parsed, &mut self.resolver, &self.root_witness)
    }

    fn expand_with_diagnostics(mut self) -> ModuleResolutionDiagnosticReport {
        let error =
            resolve_modules_with_resolver(self.parsed, &mut self.resolver, &self.root_witness)
                .err();
        let constraint_reports = self
            .resolver
            .constraint_reports
            .into_inner()
            .expect("diagnostic resolver always records constraint reports");
        ModuleResolutionDiagnosticReport {
            constraint_reports,
            error,
        }
    }
}

impl<'parsed, 'cfg, E: crate::parser::ExportParamLike>
    UpstreamRefCheckedModuleGraph<'parsed, 'cfg, E>
{
    /// The sole constructor for the upstream-only, unknown-type-deferring
    /// resolver. Ordinary callers can construct only `RefCheckedModuleGraph`.
    fn check(
        parsed: &'parsed mut crate::parser::File<E>,
        base_dir: &Path,
        config: &'cfg ProviderContext,
        schemas: &'cfg SchemaRegistry,
    ) -> Result<Self, ModuleError> {
        let mut resolver = ModuleResolver::<DeferredUpstreamTypeResolution>::for_upstream(
            base_dir, config, schemas,
        );
        resolver.process_imports(&parsed.uses, &parsed.module_calls)?;
        let root_witness = resolver.validate_reference_types(parsed)?;
        Ok(Self {
            parsed,
            resolver,
            root_witness,
        })
    }

    fn expand(mut self) -> Result<(), ModuleError> {
        resolve_modules_with_resolver(self.parsed, &mut self.resolver, &self.root_witness)
    }
}

/// Resolve all modules in a parsed file
pub fn resolve_modules(
    parsed: &mut crate::parser::ParsedFile,
    base_dir: &Path,
) -> Result<(), ModuleError> {
    resolve_modules_with_config(
        parsed,
        base_dir,
        &ProviderContext::default(),
        &SchemaRegistry::new(),
    )
}

/// Resolve all modules in a parsed file with the given parser configuration.
///
/// Generic over the export-parameter shape so callers can pass either
/// `ParsedFile` (parser phase) or `InferredFile` (post-loader phase) —
/// the resolver only touches `uses`, `module_calls`, and `resources`.
pub fn resolve_modules_with_config<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<(), ModuleError> {
    RefCheckedModuleGraph::check(parsed, base_dir, config, schemas)?.expand()
}

/// Expand an upstream project's modules using the consumer's necessarily
/// partial provider context. Unresolvable custom type names remain unchecked;
/// all concrete value and liftable boundary mismatches still use the normal
/// witness and shared value checker.
pub(crate) fn resolve_upstream_modules_with_config<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<(), ModuleError> {
    UpstreamRefCheckedModuleGraph::check(parsed, base_dir, config, schemas)?.expand()
}

/// Resolve modules while retaining the expander's own per-call constraint
/// outcomes and source locations for editor diagnostics.
pub fn resolve_modules_with_diagnostics<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    base_dir: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> ModuleResolutionDiagnosticReport {
    match RefCheckedModuleGraph::check_with_reporting(parsed, base_dir, config, schemas) {
        Ok(checked) => checked.expand_with_diagnostics(),
        Err(error) => ModuleResolutionDiagnosticReport {
            constraint_reports: IndexMap::new(),
            error: Some(error),
        },
    }
}

fn resolve_modules_with_resolver<E, P: TypeResolutionPolicy>(
    parsed: &mut crate::parser::File<E>,
    resolver: &mut ModuleResolver<'_, P>,
    ref_check: &RefCheckWitness,
) -> Result<(), ModuleError> {
    // Expand module calls. `module_calls` is cloned because the loop
    // body folds each contribution in via `merge_parsed_file(parsed,
    // …)`, a whole-`&mut parsed` borrow that conflicts with iterating
    // `&parsed.module_calls` in place.
    let module_calls = parsed.module_calls.clone();
    let signatures = resolver.resolved_module_signatures();
    let binding_index = crate::binding_index::BindingIndex::from_parsed_with_module_signatures(
        parsed,
        resolver.schemas,
        &signatures,
    );
    let resolved_bindings = crate::binding_index::ResolvedBindings::for_validation(parsed);
    let mut expanded_calls = Vec::with_capacity(module_calls.len());
    for call in &module_calls {
        let instance_prefix = instance_prefix_for_call(call);
        let expanded = resolver.expand_module_call_checked(
            call,
            &instance_prefix,
            ref_check,
            Some(&binding_index),
            Some(&resolved_bindings),
        )?;
        // `expand_module_call` returns a parser-phase `ParsedFile`;
        // this caller's `parsed` is a generic `File<E>` (the CLI
        // validate path passes an `InferredFile`). Relabel the
        // export-param-free contribution to the target phase, then
        // fold it in via the one shared merge surface so no field can
        // be silently dropped here (carina#3126 / carina#3061).
        expanded_calls.push(crate::config_loader::relabel_export_phase(expanded));
    }
    drop(binding_index);
    for expanded in expanded_calls {
        crate::config_loader::merge_parsed_file(parsed, expanded);
    }

    Ok(())
}

#[cfg(test)]
mod type_resolution_policy_tests {
    use super::*;

    fn assert_strict(_: &ModuleResolver<'_, StrictTypeResolution>) {}

    fn assert_upstream(_: &ModuleResolver<'_, DeferredUpstreamTypeResolution>) {}

    #[test]
    fn resolver_constructors_choose_distinct_type_resolution_policies() {
        let config = ProviderContext::default();
        let schemas = SchemaRegistry::new();

        let strict = ModuleResolver::with_config_and_schemas(".", &config, &schemas);
        let upstream =
            ModuleResolver::<DeferredUpstreamTypeResolution>::for_upstream(".", &config, &schemas);

        assert_strict(&strict);
        assert_upstream(&upstream);
    }
}
