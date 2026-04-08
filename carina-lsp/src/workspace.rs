//! Workspace scanning: discover and parse provider configurations from .crn files.

use std::fs;
use std::path::Path;

use carina_core::parser::{self, ProviderConfig, ProviderContext};

/// Discover all provider configurations from .crn files in a workspace directory.
///
/// Scans the directory (non-recursively) for files ending in `.crn`,
/// parses each one, and collects all `provider` blocks. Duplicate provider
/// names are deduplicated (first occurrence wins). Unreadable or unparseable
/// files are silently skipped.
pub fn discover_providers(workspace_root: &Path) -> Vec<ProviderConfig> {
    let entries = match fs::read_dir(workspace_root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut seen_names = std::collections::HashSet::new();
    let mut providers = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn")
            && path.is_file()
            && let Ok(content) = fs::read_to_string(&path)
        {
            let ctx = ProviderContext::default();
            if let Ok(parsed) = parser::parse(&content, &ctx) {
                for provider in parsed.providers {
                    if seen_names.insert(provider.name.clone()) {
                        providers.push(provider);
                    }
                }
            }
        }
    }

    providers
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
        assert_eq!(providers[0].name, "aws");
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
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
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
        assert_eq!(providers[0].name, "aws");
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
        assert_eq!(providers[0].name, "awscc");
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
    fn discover_providers_nonexistent_directory() {
        let providers = discover_providers(Path::new("/nonexistent/path"));
        assert!(providers.is_empty());
    }
}
