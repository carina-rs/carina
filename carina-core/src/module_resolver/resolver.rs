//! `ModuleResolver` driver: import processing, nested-module resolution,
//! and the top-level `resolve_modules*` entry points.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::parser::{ParsedFile, ProviderContext, UseStatement};

use super::error::ModuleError;
use super::expander::instance_prefix_for_call;
use super::loader::sorted_crn_paths_in;

/// Context for module resolution
pub struct ModuleResolver<'cfg> {
    /// Base directory for resolving relative imports
    pub(super) base_dir: PathBuf,
    /// Cache of loaded modules: path -> ParsedFile
    pub(super) module_cache: HashMap<PathBuf, ParsedFile>,
    /// Currently resolving modules (for cycle detection)
    pub(super) resolving: HashSet<PathBuf>,
    /// Imported module definitions by alias
    pub(super) imported_modules: HashMap<String, ParsedFile>,
    /// Parser configuration (decryptor, custom validators)
    pub(super) config: &'cfg ProviderContext,
}

impl<'cfg> ModuleResolver<'cfg> {
    /// Create a new resolver with the given base directory and default config
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        static DEFAULT_CONFIG: std::sync::LazyLock<ProviderContext> =
            std::sync::LazyLock::new(ProviderContext::default);
        Self::with_config(base_dir, &DEFAULT_CONFIG)
    }

    /// Create a new resolver with the given base directory and parser configuration
    pub fn with_config(base_dir: impl AsRef<Path>, config: &'cfg ProviderContext) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
            module_cache: HashMap::new(),
            resolving: HashSet::new(),
            imported_modules: HashMap::new(),
            config,
        }
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

        // Reject provider blocks inside modules
        if !parsed.providers.is_empty() {
            self.resolving.remove(&canonical);
            return Err(ModuleError::ProviderInModule);
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
    fn load_directory_module(&self, dir_path: &Path) -> Result<ParsedFile, ModuleError> {
        let mut merged = ParsedFile::default();

        for file_path in sorted_crn_paths_in(dir_path)? {
            let content = std::fs::read_to_string(&file_path)?;
            let parsed = crate::parser::parse(&content, self.config)?;
            crate::config_loader::merge_parsed_file(&mut merged, parsed);
        }

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
    pub fn process_imports(&mut self, imports: &[UseStatement]) -> Result<(), ModuleError> {
        for import in imports {
            let module = self.load_module(&import.path)?;
            self.imported_modules.insert(import.alias.clone(), module);
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
        if parsed.uses.is_empty() || parsed.module_calls.is_empty() {
            return Ok(());
        }

        // Save and temporarily replace the base_dir and imported_modules
        let original_base_dir = std::mem::replace(&mut self.base_dir, base_dir.to_path_buf());
        let original_imported = std::mem::take(&mut self.imported_modules);

        // Process the module's own imports
        let imports = parsed.uses.clone();
        let result = self.process_imports(&imports);

        if let Err(e) = result {
            // Restore state on error
            self.base_dir = original_base_dir;
            self.imported_modules = original_imported;
            return Err(e);
        }

        // Expand the module's own module_calls
        let module_calls = parsed.module_calls.clone();
        for call in &module_calls {
            let instance_prefix = instance_prefix_for_call(call);

            match self.expand_module_call(call, &instance_prefix) {
                Ok(expanded) => parsed.resources.extend(expanded), // allow: direct — module expansion, handled separately
                Err(e) => {
                    self.base_dir = original_base_dir;
                    self.imported_modules = original_imported;
                    return Err(e);
                }
            }
        }

        // Restore original state
        self.base_dir = original_base_dir;
        self.imported_modules = original_imported;

        Ok(())
    }

    /// Get an imported module by alias
    pub fn get_module(&self, alias: &str) -> Option<&ParsedFile> {
        self.imported_modules.get(alias)
    }
}

/// Resolve all modules in a parsed file
pub fn resolve_modules(parsed: &mut ParsedFile, base_dir: &Path) -> Result<(), ModuleError> {
    resolve_modules_with_config(parsed, base_dir, &ProviderContext::default())
}

/// Resolve all modules in a parsed file with the given parser configuration.
pub fn resolve_modules_with_config(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    config: &ProviderContext,
) -> Result<(), ModuleError> {
    let mut resolver = ModuleResolver::with_config(base_dir, config);

    // Process imports
    resolver.process_imports(&parsed.uses)?;

    // Expand module calls
    for call in &parsed.module_calls {
        let instance_prefix = instance_prefix_for_call(call);
        let expanded = resolver.expand_module_call(call, &instance_prefix)?;
        parsed.resources.extend(expanded); // allow: direct — module expansion, handled separately
    }

    Ok(())
}
