use std::collections::HashSet;
use std::path::{Path, PathBuf};

use carina_core::module_resolver::{self, LoadedModule};

/// One successfully loaded module in the shared depth-first module walk.
pub(crate) struct LoadedModuleAtPath {
    diagnostic_path: PathBuf,
    module_path: PathBuf,
    guard_path: PathBuf,
    loaded: LoadedModule,
}

impl LoadedModuleAtPath {
    pub(crate) fn diagnostic_path(&self) -> &Path {
        &self.diagnostic_path
    }

    pub(crate) fn module_path(&self) -> &Path {
        &self.module_path
    }

    pub(crate) fn loaded(&self) -> &LoadedModule {
        &self.loaded
    }
}

/// Modules reachable from a root parse, in depth-first pre-order.
///
/// This is the single recursive module-loading seam for static validation.
/// Duplicate-declaration reporting and module attribute type validation both
/// consume the same loaded snapshots, so their traversal order, cycle guard,
/// and path prefixes cannot drift apart.
#[derive(Default)]
pub(crate) struct ModuleWalk {
    modules: Vec<LoadedModuleAtPath>,
}

impl ModuleWalk {
    pub(crate) fn load<E>(parsed: &carina_core::parser::File<E>, base_dir: &Path) -> Self {
        let mut walk = Self::default();
        let mut visited = HashSet::new();
        for import in &parsed.uses {
            visit_module(
                &base_dir.join(&import.path),
                Path::new(&import.path),
                &mut visited,
                &mut walk.modules,
            );
        }
        walk
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &LoadedModuleAtPath> {
        self.modules.iter()
    }

    /// Find a module already loaded by this walk using the same canonical-path
    /// identity as the recursion guard.
    pub(crate) fn parsed_at(&self, module_path: &Path) -> Option<&carina_core::parser::ParsedFile> {
        let guard_path = canonical_guard_path(module_path);
        self.modules
            .iter()
            .find(|module| module.guard_path == guard_path)
            .map(|module| &module.loaded.parsed)
    }
}

fn canonical_guard_path(module_path: &Path) -> PathBuf {
    std::fs::canonicalize(module_path).unwrap_or_else(|_| module_path.to_path_buf())
}

fn visit_module(
    module_path: &Path,
    diagnostic_path: &Path,
    visited: &mut HashSet<PathBuf>,
    modules: &mut Vec<LoadedModuleAtPath>,
) {
    let guard_path = canonical_guard_path(module_path);
    if !visited.insert(guard_path.clone()) {
        return;
    }

    let Some(loaded) = module_resolver::load_module_with_diagnostics(module_path) else {
        return;
    };
    let child_paths: Vec<_> = loaded
        .parsed
        .uses
        .iter()
        .map(|import| {
            (
                module_path.join(&import.path),
                diagnostic_path.join(&import.path),
            )
        })
        .collect();
    modules.push(LoadedModuleAtPath {
        diagnostic_path: diagnostic_path.to_path_buf(),
        module_path: module_path.to_path_buf(),
        guard_path,
        loaded,
    });

    for (child_module_path, child_diagnostic_path) in child_paths {
        visit_module(&child_module_path, &child_diagnostic_path, visited, modules);
    }
}
