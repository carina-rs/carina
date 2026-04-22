//! Workspace scanning: discover provider configurations from .crn files.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use carina_core::parser::{self, ProviderConfig, ProviderContext};

/// Discover all provider configurations from .crn files in a workspace directory.
///
/// Recursively scans the directory for files ending in `.crn`,
/// parses each one, and collects all `provider` blocks. Each provider is
/// returned with the directory containing the `.crn` file it was found in.
/// Duplicate provider names are deduplicated (first occurrence wins).
/// Unreadable or unparseable files are silently skipped.
pub fn discover_providers(workspace_root: &Path) -> Vec<(PathBuf, ProviderConfig)> {
    let mut seen_names = std::collections::HashSet::new();
    let mut providers = Vec::new();
    discover_providers_recursive(workspace_root, &mut seen_names, &mut providers);
    providers
}

fn discover_providers_recursive(
    dir: &Path,
    seen_names: &mut std::collections::HashSet<String>,
    providers: &mut Vec<(PathBuf, ProviderConfig)>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            discover_providers_recursive(&path, seen_names, providers);
        } else if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
        {
            let ctx = ProviderContext::default();
            if let Ok(parsed) = parser::parse(&content, &ctx) {
                let source_dir = path.parent().unwrap_or(dir);
                for provider in parsed.providers {
                    if seen_names.insert(provider.name.clone()) {
                        providers.push((source_dir.to_path_buf(), provider));
                    }
                }
            }
        }
    }
}

/// Discover provider configurations grouped by directory.
///
/// Unlike `discover_providers` which deduplicates globally, this groups
/// providers by their source directory. Each directory is an independent
/// Carina configuration with its own set of providers.
/// Within a single directory, duplicate provider names are deduplicated.
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

    for entry in entries.flatten() {
        let path = entry.path();
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
                let seen: std::collections::HashSet<String> =
                    dir_providers.iter().map(|p| p.name.clone()).collect();
                for provider in parsed.providers {
                    if !seen.contains(&provider.name) {
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
pub fn discover_import_map(workspace_root: &Path) -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut result: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    discover_imports_recursive(workspace_root, &mut result);
    result
}

fn discover_imports_recursive(dir: &Path, result: &mut HashMap<PathBuf, Vec<PathBuf>>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            discover_imports_recursive(&path, result);
        } else if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
        {
            let ctx = ProviderContext::default();
            if let Ok(parsed) = parser::parse(&content, &ctx) {
                let caller_dir = path.parent().unwrap_or(dir);
                for import in &parsed.imports {
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
                    result.entry(module_dir).or_default().push(caller_dir);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discover_providers_from_crn_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].1.name, "aws");
        assert_eq!(providers[0].0, dir.path());
    }

    #[test]
    fn discover_providers_multiple_files() {
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

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 2);
        let names: Vec<&str> = providers.iter().map(|p| p.1.name.as_str()).collect();
        assert!(names.contains(&"aws"));
        assert!(names.contains(&"awscc"));
    }

    #[test]
    fn discover_providers_deduplicates() {
        let dir = TempDir::new().unwrap();
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

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].1.name, "aws");
    }

    #[test]
    fn discover_providers_empty_directory() {
        let dir = TempDir::new().unwrap();
        let providers = discover_providers(dir.path());
        assert!(providers.is_empty());
    }

    #[test]
    fn discover_providers_no_provider_blocks() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "aws.s3.Bucket {\n  bucket_name = 'test'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert!(providers.is_empty());
    }

    #[test]
    fn discover_providers_skips_unparseable_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bad.crn"), "this is not valid crn {{{").unwrap();
        fs::write(
            dir.path().join("good.crn"),
            "provider awscc {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].1.name, "awscc");
    }

    #[test]
    fn discover_providers_skips_non_crn_files() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("readme.md"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert!(providers.is_empty());
    }

    #[test]
    fn discover_providers_recursive_nested_directories() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("modules").join("web");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("main.crn"),
            "provider awscc {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].1.name, "awscc");
        assert_eq!(providers[0].0, nested);
    }

    #[test]
    fn discover_providers_nonexistent_directory() {
        let providers = discover_providers(Path::new("/nonexistent/path"));
        assert!(providers.is_empty());
    }

    #[test]
    fn discover_providers_returns_source_directory() {
        let dir = TempDir::new().unwrap();
        let sub_a = dir.path().join("env_a");
        let sub_b = dir.path().join("env_b");
        fs::create_dir_all(&sub_a).unwrap();
        fs::create_dir_all(&sub_b).unwrap();

        fs::write(
            sub_a.join("providers.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            sub_b.join("providers.crn"),
            "provider awscc {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 2);

        for (source_dir, config) in &providers {
            match config.name.as_str() {
                "aws" => assert_eq!(source_dir, &sub_a),
                "awscc" => assert_eq!(source_dir, &sub_b),
                other => panic!("unexpected provider: {}", other),
            }
        }
    }

    #[test]
    fn discover_providers_dedup_preserves_source_directory() {
        let dir = TempDir::new().unwrap();
        let sub_a = dir.path().join("env_a");
        let sub_b = dir.path().join("env_b");
        fs::create_dir_all(&sub_a).unwrap();
        fs::create_dir_all(&sub_b).unwrap();

        fs::write(
            sub_a.join("providers.crn"),
            "provider aws {\n  region = 'us-east-1'\n}\n",
        )
        .unwrap();
        fs::write(
            sub_b.join("providers.crn"),
            "provider aws {\n  region = 'ap-northeast-1'\n}\n",
        )
        .unwrap();

        let providers = discover_providers(dir.path());
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].1.name, "aws");
        // Source directory should be one of the two (readdir order is not guaranteed)
        assert!(
            providers[0].0 == sub_a || providers[0].0 == sub_b,
            "source_dir should be one of the subdirectories, got: {:?}",
            providers[0].0
        );
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
            "let github = import '../../modules/github-oidc'\n",
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
    fn discover_import_map_empty_workspace() {
        let dir = TempDir::new().unwrap();
        let import_map = discover_import_map(dir.path());
        assert!(import_map.is_empty());
    }
}
