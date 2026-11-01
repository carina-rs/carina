use std::collections::HashSet;
use std::path::{Path, PathBuf};

use carina_core::module_resolver::{self, LoadedModule};
use carina_core::paths::{CanonicalPath, ResolvedPath};

/// One successfully loaded module in the shared depth-first module walk.
pub(crate) struct LoadedModuleAtPath {
    diagnostic_path: PathBuf,
    module_path: ResolvedPath,
    loaded: LoadedModule,
}

impl LoadedModuleAtPath {
    pub(crate) fn diagnostic_path(&self) -> &Path {
        &self.diagnostic_path
    }

    pub(crate) fn module_path(&self) -> &ResolvedPath {
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
    pub(crate) fn load<E>(parsed: &carina_core::parser::File<E>, base_dir: &ResolvedPath) -> Self {
        let mut walk = Self::default();
        let base_dir = base_dir.clone();
        let mut visited = HashSet::new();
        for import in &parsed.uses {
            let Ok(module_path) = base_dir.join(&import.path) else {
                continue;
            };
            visit_module(
                &module_path,
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
}

fn visit_module(
    module_path: &ResolvedPath,
    diagnostic_path: &Path,
    visited: &mut HashSet<CanonicalPath>,
    modules: &mut Vec<LoadedModuleAtPath>,
) {
    if !visited.insert(module_path.identity()) {
        return;
    }

    let Some(loaded) = module_resolver::load_module_with_diagnostics(module_path) else {
        return;
    };
    let child_paths: Vec<_> = loaded
        .parsed
        .uses
        .iter()
        .filter_map(|import| {
            Some((
                module_path.join(&import.path).ok()?,
                diagnostic_path.join(&import.path),
            ))
        })
        .collect();
    modules.push(LoadedModuleAtPath {
        diagnostic_path: diagnostic_path.to_path_buf(),
        module_path: module_path.clone(),
        loaded,
    });

    for (child_module_path, child_diagnostic_path) in child_paths {
        visit_module(&child_module_path, &child_diagnostic_path, visited, modules);
    }
}
