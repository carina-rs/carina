use std::collections::HashSet;
use std::path::{Path, PathBuf};

use carina_core::module_resolver::{
    self, ModuleBodyAnalysis, ModuleError, ModuleLoadError, ModuleProhibitionCategory,
    ModuleRejection, PartialModule, PartialModuleAnalysis,
};
use carina_core::paths::{CanonicalPath, DisplayPath, ResolvedPath};

/// One successfully loaded module in the shared depth-first module walk.
pub(crate) struct LoadedModuleAtPath {
    // Retain the import chain for provenance; render diagnostic_label() for users.
    diagnostic_path: PathBuf,
    diagnostic_base: ResolvedPath,
    module_path: ResolvedPath,
    loaded: PartialModule,
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

    pub(crate) fn loaded(&self) -> &PartialModule {
        &self.loaded
    }
}

pub(crate) struct RejectedModuleAtPath {
    pub(crate) label: DisplayPath,
    pub(crate) error: std::sync::Arc<ModuleError>,
    pub(crate) category: Option<ModuleProhibitionCategory>,
}

impl RejectedModuleAtPath {
    pub(crate) fn diagnostic(&self) -> crate::error::AppError {
        crate::error::AppError::ModuleImport {
            path: self.label.clone(),
            error: self.error.clone(),
        }
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
            let owners = parsed
                .module_calls
                .iter()
                .filter(|call| call.module_name == import.alias)
                .map(|call| call.id().clone())
                .collect::<Vec<_>>();
            visit_module(
                ModuleVisit {
                    module_path: &module_path,
                    diagnostic_path: Path::new(&import.path),
                    diagnostic_base: &base_dir,
                    owners: &owners,
                    ordinary_owners: &owners,
                    import_location: import.declaration_location(),
                },
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

struct ModuleVisit<'a> {
    module_path: &'a ResolvedPath,
    diagnostic_path: &'a Path,
    diagnostic_base: &'a ResolvedPath,
    owners: &'a [carina_core::parser::ModuleCallId],
    ordinary_owners: &'a [carina_core::parser::ModuleCallId],
    import_location: &'a carina_core::parser::DeclarationLocation,
}

fn visit_module(
    ModuleVisit {
        module_path,
        diagnostic_path,
        diagnostic_base,
        owners,
        ordinary_owners,
        import_location,
    }: ModuleVisit<'_>,
    visited: &mut HashSet<CanonicalPath>,
    walk: &mut ModuleWalk,
    context: &carina_core::parser::ProviderContext,
    schemas: &carina_core::schema::SchemaRegistry,
) {
    if !visited.insert(module_path.identity()) {
        return;
    }

    let mut child_calls =
        std::collections::HashMap::<String, Vec<carina_core::parser::ModuleCallId>>::new();
    let imports = match module_resolver::load_partial_module_analysis_with_schemas(
        module_path,
        context,
        schemas,
        &diagnostic_base.source_label(),
    ) {
        Ok(PartialModuleAnalysis {
            imports,
            prohibitions,
            body,
            signature: _,
        }) => {
            if let Some(prohibitions) = prohibitions {
                walk.rejections.extend(
                    prohibitions
                        .into_owned_errors(import_location.clone(), owners.to_vec())
                        .into_iter()
                        .map(|(category, error)| RejectedModuleAtPath {
                            label: DisplayPath::for_diagnostics_from(
                                diagnostic_path,
                                diagnostic_base,
                            ),
                            error: std::sync::Arc::new(error),
                            category: Some(category),
                        }),
                );
            }
            match body {
                ModuleBodyAnalysis::Available(loaded) => {
                    for call in &loaded.parsed().module_calls {
                        child_calls
                            .entry(call.module_name.clone())
                            .or_default()
                            .push(call.id().clone());
                    }
                    walk.modules.push(LoadedModuleAtPath {
                        diagnostic_path: diagnostic_path.to_path_buf(),
                        diagnostic_base: diagnostic_base.clone(),
                        module_path: module_path.clone(),
                        loaded,
                    });
                }
                ModuleBodyAnalysis::ProhibitionsOnly | ModuleBodyAnalysis::Unavailable => {}
            }
            imports
        }
        Err(ModuleLoadError::Partial {
            prohibitions,
            imports,
            signature: _,
        }) => {
            walk.rejections.extend(
                prohibitions
                    .into_owned_errors(import_location.clone(), owners.to_vec())
                    .into_iter()
                    .map(|(category, error)| RejectedModuleAtPath {
                        label: DisplayPath::for_diagnostics_from(diagnostic_path, diagnostic_base),
                        error: std::sync::Arc::new(error),
                        category: Some(category),
                    }),
            );
            imports
        }
        Err(ModuleLoadError::Unavailable(_)) => return,
        Err(ModuleLoadError::NotModule {
            error,
            imports,
            signature: _,
        }) => {
            match *error {
                ModuleRejection::NotADirectory { path: _ } => return,
                rejection @ ModuleRejection::NoConfigurationFiles { path: _ } => {
                    walk.rejections
                        .extend(rejection.into_errors().into_iter().map(|error| {
                            RejectedModuleAtPath {
                                label: DisplayPath::for_diagnostics_from(
                                    diagnostic_path,
                                    diagnostic_base,
                                ),
                                error: std::sync::Arc::new(error.with_import_owners(
                                    import_location.clone(),
                                    ordinary_owners.to_vec(),
                                )),
                                category: None,
                            }
                        }));
                }
                ModuleRejection::Prohibited(prohibitions) => {
                    walk.rejections.extend(
                        prohibitions
                            .into_owned_errors(import_location.clone(), owners.to_vec())
                            .into_iter()
                            .map(|(category, error)| RejectedModuleAtPath {
                                label: DisplayPath::for_diagnostics_from(
                                    diagnostic_path,
                                    diagnostic_base,
                                ),
                                error: std::sync::Arc::new(error),
                                category: Some(category),
                            }),
                    );
                }
            }
            imports
        }
    };
    let child_paths: Vec<_> = imports
        .iter()
        .filter_map(|import| {
            Some((
                module_path.join(&import.path).ok()?,
                diagnostic_path.join(&import.path),
                if ordinary_owners.is_empty() {
                    child_calls.remove(&import.alias).unwrap_or_default()
                } else {
                    ordinary_owners.to_vec()
                },
            ))
        })
        .collect();

    for (child_module_path, child_diagnostic_path, child_ordinary_owners) in child_paths {
        visit_module(
            ModuleVisit {
                module_path: &child_module_path,
                diagnostic_path: &child_diagnostic_path,
                diagnostic_base,
                owners,
                ordinary_owners: &child_ordinary_owners,
                import_location,
            },
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
    fn empty_import_is_a_typed_static_rejection_without_a_use_anchor() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("not-a-directory.crn"), "// file\n").unwrap();
        std::fs::create_dir(directory.path().join("empty")).unwrap();
        let environment = carina_core::EnvironmentSnapshot::empty();
        let context = carina_core::parser::ProviderContext::new(environment.clone());
        let parsed = carina_core::parser::parse_file(
            "let file = use { source = './not-a-directory.crn' }\nlet empty = use { source = './empty' }\n",
            &directory.path().join("main.crn"), &context,
        ).unwrap();
        let walk = ModuleWalk::load(
            &parsed,
            &ResolvedPath::new(directory.path()).unwrap(),
            &environment,
            &carina_core::schema::SchemaRegistry::new(),
        );
        assert_eq!(walk.rejections().len(), 1);
        let rejection = &walk.rejections()[0];
        assert!(matches!(
            rejection.error.root_cause(),
            module_resolver::ModuleError::NoConfigurationFiles { path: _ }
        ));
        assert!(matches!(
            rejection.error.call_source(),
            module_resolver::ModuleErrorCallSource::Unavailable
        ));
        assert_eq!(crate::wiring::validate_module_boundaries(&walk).len(), 1);
    }

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
            ModuleError::ProviderInModule { evidence: _ }
        ));
        assert!(matches!(
            walk.rejections()[1].error.root_cause(),
            ModuleError::BackendInModule { evidence: _ }
        ));
    }
}
