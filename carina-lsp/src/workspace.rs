//! Workspace scanning: discover provider configurations from .crn files.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use carina_core::parser::{self, ProviderConfig, ProviderContext};

#[derive(PartialEq, Eq, Hash)]
struct ProviderInstanceKey {
    kind: String,
    binding: Option<String>,
}

impl From<&ProviderConfig> for ProviderInstanceKey {
    fn from(config: &ProviderConfig) -> Self {
        Self {
            kind: config.name.clone(),
            binding: config.binding.clone(),
        }
    }
}

/// Discover provider configurations grouped by directory.
///
/// Each directory is an independent Carina configuration with its own set of
/// providers. Within a directory, duplicate provider instances are identified
/// by provider kind and optional binding; the first declaration in sorted path
/// order wins.
pub fn discover_providers_by_dir(workspace_root: &Path) -> HashMap<PathBuf, Vec<ProviderConfig>> {
    let mut result: HashMap<PathBuf, Vec<ProviderConfig>> = HashMap::new();
    discover_by_dir_recursive(workspace_root, &mut result);
    result
}

fn discover_by_dir_recursive(dir: &Path, result: &mut HashMap<PathBuf, Vec<ProviderConfig>>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if path.is_dir() {
            discover_by_dir_recursive(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
        {
            let ctx = ProviderContext::default();
            if let Ok(parsed) = parser::parse(&content, &ctx)
                && !parsed.providers.is_empty()
            {
                let source_dir = path.parent().unwrap_or(dir).to_path_buf();
                let dir_providers = result.entry(source_dir).or_default();
                let mut seen: HashSet<ProviderInstanceKey> = dir_providers
                    .iter()
                    .map(ProviderInstanceKey::from)
                    .collect();
                for provider in parsed.providers {
                    if seen.insert(ProviderInstanceKey::from(&provider)) {
                        dir_providers.push(provider);
                    }
                }
            }
        }
    }
}

/// Build a reverse import map: module directory → set of caller directories.
///
/// Scans all `.crn` files in the workspace for `import` statements, resolves
/// the relative paths to absolute module directories, and maps each module
/// to the directories that import it. This allows module files to inherit
/// their callers' provider schemas.
pub fn discover_import_map(workspace_root: &Path) -> HashMap<PathBuf, BTreeSet<PathBuf>> {
    let mut result: HashMap<PathBuf, BTreeSet<PathBuf>> = HashMap::new();
    discover_imports_recursive(workspace_root, &mut result);
    result
}

fn discover_imports_recursive(dir: &Path, result: &mut HashMap<PathBuf, BTreeSet<PathBuf>>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if path.is_dir() {
            discover_imports_recursive(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
        {
            let ctx = ProviderContext::default();
            if let Ok(parsed) = parser::parse(&content, &ctx) {
                let caller_dir = path.parent().unwrap_or(dir);
                for import in &parsed.uses {
                    let module_path = caller_dir.join(&import.path);
                    // Resolve to canonical directory (strip .crn extension, handle dirs)
                    let module_dir = if module_path.is_dir() {
                        module_path
                    } else if module_path.extension().is_some_and(|ext| ext == "crn") {
                        module_path.parent().unwrap_or(&module_path).to_path_buf()
                    } else {
                        // Try with .crn extension
                        let with_ext = module_path.with_extension("crn");
                        if with_ext.exists() {
                            with_ext.parent().unwrap_or(&module_path).to_path_buf()
                        } else {
                            // Might be a directory module
                            module_path
                        }
                    };
                    // Canonicalize to resolve .. and symlinks
                    let module_dir = module_dir.canonicalize().unwrap_or(module_dir);
                    let caller_dir = caller_dir
                        .canonicalize()
                        .unwrap_or(caller_dir.to_path_buf());
                    result.entry(module_dir).or_default().insert(caller_dir);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    #[test]
    fn discover_by_dir_from_crn_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[dir.path()];
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "aws");
    }

    #[test]
    fn discover_by_dir_multiple_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.crn"),
            "provider awscc {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[dir.path()];
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].name, "aws");
        assert_eq!(providers[1].name, "awscc");
    }

    #[test]
    fn discover_by_dir_no_provider_blocks() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "aws.s3.Bucket {\n  bucket_name = 'test'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        assert!(by_dir.is_empty());
    }

    #[test]
    fn discover_by_dir_skips_unparseable_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bad.crn"), "this is not valid crn {{{").unwrap();
        fs::write(
            dir.path().join("good.crn"),
            "provider awscc {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[dir.path()];
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "awscc");
    }

    #[test]
    fn discover_by_dir_skips_non_crn_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("readme.md"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        assert!(by_dir.is_empty());
    }

    #[test]
    fn discover_by_dir_recurses_into_nested_directories() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("modules").join("web");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("main.crn"),
            "provider awscc {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[&nested];
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "awscc");
    }

    #[test]
    fn discover_by_dir_nonexistent_directory() {
        let by_dir = discover_providers_by_dir(Path::new("/nonexistent/path"));
        assert!(by_dir.is_empty());
    }

    #[test]
    fn discover_by_dir_groups_by_directory() {
        let dir = TempDir::new().unwrap();
        let env_a = dir.path().join("env_a");
        let env_b = dir.path().join("env_b");
        fs::create_dir_all(&env_a).unwrap();
        fs::create_dir_all(&env_b).unwrap();

        fs::write(
            env_a.join("providers.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            env_b.join("providers.crn"),
            "provider awscc {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        assert_eq!(by_dir.len(), 2);
        assert_eq!(by_dir[&env_a].len(), 1);
        assert_eq!(by_dir[&env_a][0].name, "aws");
        assert_eq!(by_dir[&env_b].len(), 1);
        assert_eq!(by_dir[&env_b][0].name, "awscc");
    }

    #[test]
    fn discover_by_dir_same_provider_in_different_dirs_not_deduplicated() {
        let dir = TempDir::new().unwrap();
        let env_a = dir.path().join("env_a");
        let env_b = dir.path().join("env_b");
        fs::create_dir_all(&env_a).unwrap();
        fs::create_dir_all(&env_b).unwrap();

        // Same provider name in two directories — both should appear
        fs::write(
            env_a.join("providers.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            env_b.join("providers.crn"),
            "provider aws {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        assert_eq!(by_dir.len(), 2);
        assert!(by_dir.contains_key(&env_a));
        assert!(by_dir.contains_key(&env_b));
    }

    #[test]
    fn discover_by_dir_deduplicates_within_same_directory() {
        let dir = TempDir::new().unwrap();
        // Two files in the same directory both declare provider aws
        fs::write(
            dir.path().join("a.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.crn"),
            "provider aws {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        assert_eq!(by_dir.len(), 1);
        assert_eq!(by_dir[dir.path()].len(), 1);
        assert_eq!(by_dir[dir.path()][0].name, "aws");
    }

    #[test]
    fn discover_by_dir_keeps_default_and_named_instance_in_both_file_orders() {
        for (default_file, named_file) in [
            ("a_default.crn", "z_named.crn"),
            ("z_default.crn", "a_named.crn"),
        ] {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join(default_file),
                "provider aws {\n  source = 'file:///x.wasm'\n}\n",
            )
            .unwrap();
            fs::write(
                dir.path().join(named_file),
                "let east = provider aws {\n  region = 'us-east-1'\n}\n",
            )
            .unwrap();

            let by_dir = discover_providers_by_dir(dir.path());
            let providers = &by_dir[dir.path()];

            assert_eq!(
                providers.len(),
                2,
                "default and named instances must both survive when default={default_file}, named={named_file}"
            );
            assert!(providers.iter().any(|provider| {
                provider.is_default
                    && provider.binding.is_none()
                    && provider.source.as_deref() == Some("file:///x.wasm")
            }));
            assert!(providers.iter().any(|provider| {
                !provider.is_default && provider.binding.as_deref() == Some("east")
            }));
        }
    }

    #[test]
    fn discover_by_dir_keeps_multiple_named_instances_of_same_kind() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("a_east.crn"),
            "let east = provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("z_west.crn"),
            "let west = provider aws {\n  region = 'us-west-2'\n}\n",
        )
        .unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let bindings: Vec<&str> = by_dir[dir.path()]
            .iter()
            .filter_map(|provider| provider.binding.as_deref())
            .collect();

        assert_eq!(bindings, ["east", "west"]);
    }

    #[test]
    fn discover_by_dir_keeps_defaults_of_different_kinds() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a_aws.crn"), "provider aws {}\n").unwrap();
        fs::write(dir.path().join("z_awscc.crn"), "provider awscc {}\n").unwrap();

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[dir.path()];

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].name, "aws");
        assert_eq!(providers[1].name, "awscc");
        assert!(
            providers
                .iter()
                .all(|provider| provider.is_default && provider.binding.is_none())
        );
    }

    #[test]
    fn discover_by_dir_duplicate_defaults_keep_first_sorted_file() {
        let dir = TempDir::new().unwrap();
        for index in (0..10).rev() {
            fs::write(
                dir.path().join(format!("{index:02}_default.crn")),
                format!("provider aws {{\n  source = 'file:///{index:02}.wasm'\n}}\n"),
            )
            .unwrap();
        }

        let by_dir = discover_providers_by_dir(dir.path());
        let providers = &by_dir[dir.path()];

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].source.as_deref(), Some("file:///00.wasm"));
    }

    #[test]
    fn discover_by_dir_empty_workspace() {
        let dir = TempDir::new().unwrap();
        let by_dir = discover_providers_by_dir(dir.path());
        assert!(by_dir.is_empty());
    }

    #[test]
    fn discover_import_map_finds_module_callers() {
        let dir = TempDir::new().unwrap();
        let caller = dir.path().join("aws").join("github-oidc");
        let module = dir.path().join("modules").join("github-oidc");
        fs::create_dir_all(&caller).unwrap();
        fs::create_dir_all(&module).unwrap();

        // Caller imports the module
        fs::write(
            caller.join("main.crn"),
            "let github = use { source = '../../modules/github-oidc' }\n",
        )
        .unwrap();
        // Module has arguments (no provider)
        fs::write(module.join("main.crn"), "arguments {\n  repo: String\n}\n").unwrap();

        let import_map = discover_import_map(dir.path());

        let module_canonical = module.canonicalize().unwrap();
        assert!(
            import_map.contains_key(&module_canonical),
            "import_map should contain module dir. Keys: {:?}",
            import_map.keys().collect::<Vec<_>>()
        );

        let callers = &import_map[&module_canonical];
        let caller_canonical = caller.canonicalize().unwrap();
        assert!(
            callers.contains(&caller_canonical),
            "callers should contain the caller dir. Got: {:?}",
            callers
        );
    }

    #[test]
    fn discover_import_map_sorts_and_deduplicates_callers() {
        let dir = TempDir::new().unwrap();
        let module = dir.path().join("modules").join("shared");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("main.crn"), "arguments {\n  value: String\n}\n").unwrap();

        let callers_root = dir.path().join("callers");
        for name in ["zeta", "middle", "alpha"] {
            let caller = callers_root.join(name);
            fs::create_dir_all(&caller).unwrap();
            let import = "let shared = use { source = '../../modules/shared' }\n";
            fs::write(caller.join("z_import.crn"), import).unwrap();
            fs::write(caller.join("a_import.crn"), import).unwrap();
        }

        let import_map: HashMap<PathBuf, BTreeSet<PathBuf>> = discover_import_map(dir.path());
        let module = module.canonicalize().unwrap();
        let expected: BTreeSet<PathBuf> = ["alpha", "middle", "zeta"]
            .into_iter()
            .map(|name| callers_root.join(name).canonicalize().unwrap())
            .collect();

        assert_eq!(import_map[&module], expected);
    }

    #[test]
    fn discover_import_map_empty_workspace() {
        let dir = TempDir::new().unwrap();
        let import_map = discover_import_map(dir.path());
        assert!(import_map.is_empty());
    }
}
