use std::collections::HashSet;
use std::path::{Path, PathBuf};

use carina_core::module_resolver::{self, LoadedModule};
use carina_core::paths::{CanonicalPath, DisplayPath, ResolvedPath};

/// One successfully loaded module in the shared depth-first module walk.
pub(crate) struct LoadedModuleAtPath {
    // Retain the import chain for provenance; render diagnostic_label() for users.
    diagnostic_path: PathBuf,
    diagnostic_base: ResolvedPath,
    module_path: ResolvedPath,
    loaded: LoadedModule,
}

impl LoadedModuleAtPath {
    pub(crate) fn diagnostic_path(&self) -> &Path {
        &self.diagnostic_path
    }

    pub(crate) fn diagnostic_label(&self) -> DisplayPath {
        DisplayPath::for_diagnostics_from(self.diagnostic_path(), &self.diagnostic_base)
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
    pub(crate) fn load<E>(
        parsed: &carina_core::parser::File<E>,
        base_dir: &ResolvedPath,
        environment: &carina_core::EnvironmentSnapshot,
        schemas: &carina_core::schema::SchemaRegistry,
    ) -> Self {
        let context = carina_core::parser::ProviderContext::new(environment.clone());
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
                &base_dir,
                &mut visited,
                &mut walk.modules,
                &context,
                schemas,
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
    diagnostic_base: &ResolvedPath,
    visited: &mut HashSet<CanonicalPath>,
    modules: &mut Vec<LoadedModuleAtPath>,
    context: &carina_core::parser::ProviderContext,
    schemas: &carina_core::schema::SchemaRegistry,
) {
    if !visited.insert(module_path.identity()) {
        return;
    }

    let Some(loaded) =
        module_resolver::load_module_with_diagnostics_and_schemas(module_path, context, schemas)
    else {
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
        diagnostic_base: diagnostic_base.clone(),
        module_path: module_path.clone(),
        loaded,
    });

    for (child_module_path, child_diagnostic_path) in child_paths {
        visit_module(
            &child_module_path,
            &child_diagnostic_path,
            diagnostic_base,
            visited,
            modules,
            context,
            schemas,
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn module_walk_preserves_symlink_parent_diagnostic_spelling() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let private = temp.path().join("private");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(private.join("nested")).unwrap();
        std::fs::create_dir_all(private.join("bad/child")).unwrap();
        std::os::unix::fs::symlink(private.join("nested"), project.join("jump")).unwrap();
        std::fs::write(
            private.join("bad/main.crn"),
            "arguments { value: String }\nlet child = use { source = './child' }\nchild { value = value }\n",
        )
        .unwrap();
        std::fs::write(
            private.join("bad/child/main.crn"),
            "arguments { value: String }\n",
        )
        .unwrap();
        let context =
            carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let parsed = carina_core::parser::parse_file(
            "let bad = use { source = './jump/../bad' }\nbad { value = 'hello' }\n",
            &project.join("main.crn"),
            &context,
        )
        .unwrap();
        let walk = ModuleWalk::load(
            &parsed,
            &ResolvedPath::new(&project).unwrap(),
            &carina_core::EnvironmentSnapshot::empty(),
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert_eq!(
            walk.iter()
                .map(|module| module.diagnostic_path())
                .collect::<Vec<_>>(),
            vec![
                Path::new("./jump/../bad"),
                Path::new("./jump/../bad/./child")
            ]
        );
        assert_eq!(
            walk.iter()
                .map(|module| module.diagnostic_label().to_string())
                .collect::<Vec<_>>(),
            vec!["jump/../bad", "jump/../bad/child"]
        );
    }
}
