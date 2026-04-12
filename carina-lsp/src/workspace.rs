//! Workspace scanning: discover provider configurations from .crn files.

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
            "aws.s3.bucket {\n  bucket_name = 'test'\n}\n",
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
}
