use std::collections::HashSet;
use std::path::{Path, PathBuf};

use carina_core::module_resolver::{self, LoadedModule, ModuleError, ModuleLoadError};
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

pub(crate) struct RejectedModuleAtPath {
    pub(crate) label: DisplayPath,
    pub(crate) error: std::sync::Arc<ModuleError>,
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
    rejections: Vec<RejectedModuleAtPath>,
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
                &mut walk,
                &context,
                schemas,
            );
        }
        walk
    }

    pub(crate) fn rejections(&self) -> &[RejectedModuleAtPath] {
        &self.rejections
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
    walk: &mut ModuleWalk,
    context: &carina_core::parser::ProviderContext,
    schemas: &carina_core::schema::SchemaRegistry,
) {
    if !visited.insert(module_path.identity()) {
        return;
    }

    let imports = match module_resolver::load_module_with_diagnostics_and_schemas(
        module_path,
        context,
        schemas,
        &diagnostic_base.source_label(),
    ) {
        Ok(loaded) => {
            let imports = loaded.parsed.uses.clone();
            walk.modules.push(LoadedModuleAtPath {
                diagnostic_path: diagnostic_path.to_path_buf(),
                diagnostic_base: diagnostic_base.clone(),
                module_path: module_path.clone(),
                loaded,
            });
            imports
        }
        // Preserve the existing static walk policy for unreadable/broken
        // modules. The strict resolver still reports these typed failures.
        Err(ModuleLoadError::Unavailable(_)) => return,
        Err(ModuleLoadError::NotModule { error, imports }) => {
            walk.rejections.extend(
                ModuleLoadError::NotModule {
                    error,
                    imports: Vec::new(),
                }
                .into_errors()
                .into_iter()
                .map(|error| RejectedModuleAtPath {
                    label: DisplayPath::for_diagnostics_from(diagnostic_path, diagnostic_base),
                    error: std::sync::Arc::new(error),
                }),
            );
            imports
        }
    };
    let child_paths: Vec<_> = imports
        .iter()
        .filter_map(|import| {
            Some((
                module_path.join(&import.path).ok()?,
                diagnostic_path.join(&import.path),
            ))
        })
        .collect();

    for (child_module_path, child_diagnostic_path) in child_paths {
        visit_module(
            &child_module_path,
            &child_diagnostic_path,
            diagnostic_base,
            visited,
            walk,
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

#[cfg(test)]
mod unavailable_tests {
    use super::*;
    #[test]
    fn unavailable_imports_are_explicitly_skipped_by_static_walk_and_signatures() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("arguments.crn"), "arguments {}\n").unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let child = use { source = './child' }\nlet call = child {}\n",
            &directory.path().join("main.crn"),
            &context,
        )
        .unwrap();
        let path = ResolvedPath::new(directory.path()).unwrap();
        for source in [b"let broken =\n".as_slice(), &[0xff]] {
            std::fs::write(child.join("broken.crn"), source).unwrap();
            let walk = ModuleWalk::load(
                &parsed,
                &path,
                &environment,
                &carina_core::schema::SchemaRegistry::new(),
            );
            assert_eq!(walk.iter().count(), 0);
            assert!(walk.rejections().is_empty());
            assert!(
                module_resolver::load_resolved_module_signatures(&parsed, &path, &context)
                    .is_empty()
            );
        }
    }
}

#[cfg(test)]
mod rejected_import_tests {
    use super::*;
    #[test]
    fn rejected_import_still_walks_its_nested_imports_without_exposing_placement_findings() {
        let directory = tempfile::tempdir().unwrap();
        let child = directory.path().join("child");
        std::fs::create_dir_all(child.join("nested")).unwrap();
        std::fs::write(child.join("arguments.crn"), "arguments {}\n").unwrap();
        std::fs::write(child.join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            child.join("main.crn"),
            "let nested = use { source = './nested' }\n",
        )
        .unwrap();
        std::fs::write(
            child.join("nested/backend.crn"),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let child = use { source = './child' }\n",
            &directory.path().join("main.crn"),
            &context,
        )
        .unwrap();
        let walk = ModuleWalk::load(
            &parsed,
            &ResolvedPath::new(directory.path()).unwrap(),
            &environment,
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert_eq!(walk.iter().count(), 0);
        assert_eq!(walk.rejections().len(), 2);
        assert!(matches!(
            walk.rejections()[0].error.root_cause(),
            ModuleError::ProviderInModule
        ));
        assert!(matches!(
            walk.rejections()[1].error.root_cause(),
            ModuleError::BackendInModule
        ));
    }
}
