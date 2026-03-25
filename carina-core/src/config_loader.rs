//! Configuration loading and .crn file discovery utilities

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::parser::{self, ParsedFile};

/// Result of loading configuration, includes the file path containing backend block
pub struct LoadedConfig {
    pub parsed: ParsedFile,
    /// Resources before reference resolution, for unused binding detection.
    /// After `resolve_resource_refs`, intermediate `ResourceRef` values are resolved away,
    /// so this preserves the original references for accurate unused binding analysis.
    pub unresolved_parsed: ParsedFile,
    pub backend_file: Option<PathBuf>,
}

/// Load configuration from a file or directory
pub fn load_configuration(path: &PathBuf) -> Result<LoadedConfig, String> {
    if path.is_file() {
        // Single file mode (existing behavior)
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let mut parsed = parser::parse(&content).map_err(|e| format!("Parse error: {}", e))?;
        let unresolved_parsed = parsed.clone();
        parser::resolve_resource_refs(&mut parsed).map_err(|e| format!("Parse error: {}", e))?;
        let backend_file = if parsed.backend.is_some() {
            Some(path.clone())
        } else {
            None
        };
        Ok(LoadedConfig {
            parsed,
            unresolved_parsed,
            backend_file,
        })
    } else if path.is_dir() {
        // Directory mode
        let files = find_crn_files_in_dir(path)?;
        if files.is_empty() {
            return Err(format!("No .crn files found in {}", path.display()));
        }

        let empty_parsed = || ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![],
            backend: None,
        };
        let mut merged = empty_parsed();
        let mut unresolved_merged = empty_parsed();
        let mut parse_errors = Vec::new();
        let mut backend_file: Option<PathBuf> = None;

        for file in &files {
            let content = fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            match parser::parse(&content) {
                Ok(mut parsed) => {
                    let unresolved = parsed.clone();
                    if let Err(e) = parser::resolve_resource_refs(&mut parsed) {
                        parse_errors.push(format!("{}: {}", file.display(), e));
                        continue;
                    }

                    // Merge unresolved
                    unresolved_merged.providers.extend(unresolved.providers);
                    unresolved_merged.resources.extend(unresolved.resources);
                    unresolved_merged.variables.extend(unresolved.variables);
                    unresolved_merged.imports.extend(unresolved.imports);
                    unresolved_merged
                        .module_calls
                        .extend(unresolved.module_calls);
                    unresolved_merged.arguments.extend(unresolved.arguments);
                    unresolved_merged
                        .attribute_params
                        .extend(unresolved.attribute_params);

                    // Merge resolved
                    merged.providers.extend(parsed.providers);
                    merged.resources.extend(parsed.resources);
                    merged.variables.extend(parsed.variables);
                    merged.imports.extend(parsed.imports);
                    merged.module_calls.extend(parsed.module_calls);
                    merged.arguments.extend(parsed.arguments);
                    merged.attribute_params.extend(parsed.attribute_params);
                    // Merge backend (only one allowed)
                    if let Some(backend) = parsed.backend {
                        if merged.backend.is_some() {
                            parse_errors.push(format!(
                                "{}: multiple backend blocks defined",
                                file.display()
                            ));
                        } else {
                            merged.backend = Some(backend);
                            backend_file = Some(file.clone());
                        }
                    }
                }
                Err(e) => {
                    parse_errors.push(format!("{}: {}", file.display(), e));
                }
            }
        }

        if !parse_errors.is_empty() {
            return Err(parse_errors.join("\n"));
        }
        Ok(LoadedConfig {
            parsed: merged,
            unresolved_parsed: unresolved_merged,
            backend_file,
        })
    } else {
        Err(format!("Path not found: {}", path.display()))
    }
}

/// Get base directory for module resolution
pub fn get_base_dir(path: &Path) -> &Path {
    if path.is_file() {
        path.parent().unwrap_or(Path::new("."))
    } else {
        path
    }
}

/// Find all .crn files recursively in a directory, skipping hidden dirs, target, and node_modules
pub fn find_crn_files_recursive(dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_crn_files_recursive(dir, &mut files)?;
    Ok(files)
}

fn collect_crn_files_recursive(dir: &PathBuf, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        if path.is_dir() {
            // Skip hidden directories and common non-source directories
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_crn_files_recursive(&path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }

    Ok(())
}

/// Find .crn files in a single directory (non-recursive)
pub fn find_crn_files_in_dir(dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a temporary directory for tests
    fn create_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("carina_config_loader_test_{}", name));
        // Clean up if it exists from a previous run
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Helper to clean up a temporary directory
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    // ========== find_crn_files_in_dir tests ==========

    #[test]
    fn find_crn_files_in_dir_empty_directory() {
        let dir = create_temp_dir("in_dir_empty");
        let result = find_crn_files_in_dir(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_with_crn_files() {
        let dir = create_temp_dir("in_dir_with_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.crn"), "").unwrap();

        let mut result = find_crn_files_in_dir(&dir).unwrap();
        result.sort();
        assert_eq!(result.len(), 2);
        assert!(result[0].ends_with("a.crn"));
        assert!(result[1].ends_with("b.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_ignores_non_crn_files() {
        let dir = create_temp_dir("in_dir_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.rs"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_does_not_recurse() {
        let dir = create_temp_dir("in_dir_no_recurse");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("top.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_dir_xyz");
        let result = find_crn_files_in_dir(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    // ========== find_crn_files_recursive tests ==========

    #[test]
    fn find_crn_files_recursive_empty_directory() {
        let dir = create_temp_dir("recursive_empty");
        let result = find_crn_files_recursive(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_finds_nested_files() {
        let dir = create_temp_dir("recursive_nested");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();
        let deep = sub.join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.crn"), "").unwrap();

        let mut result = find_crn_files_recursive(&dir).unwrap();
        result.sort();
        assert_eq!(result.len(), 3);
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_hidden_directories() {
        let dir = create_temp_dir("recursive_hidden");
        fs::write(dir.join("visible.crn"), "").unwrap();
        let hidden = dir.join(".hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("secret.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("visible.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_target_directory() {
        let dir = create_temp_dir("recursive_target");
        fs::write(dir.join("main.crn"), "").unwrap();
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("build.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("main.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_node_modules() {
        let dir = create_temp_dir("recursive_node_modules");
        fs::write(dir.join("app.crn"), "").unwrap();
        let nm = dir.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("dep.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("app.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_ignores_non_crn_files() {
        let dir = create_temp_dir("recursive_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.json"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_recursive_nonexistent_xyz");
        let result = find_crn_files_recursive(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    // ========== get_base_dir tests ==========

    #[test]
    fn get_base_dir_for_file() {
        let dir = create_temp_dir("base_dir_file");
        let file = dir.join("test.crn");
        fs::write(&file, "").unwrap();

        let base = get_base_dir(&file);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_directory() {
        let dir = create_temp_dir("base_dir_directory");

        let base = get_base_dir(&dir);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_nonexistent_path() {
        // Non-existent path is neither file nor dir, so returns the path itself
        let path = Path::new("/tmp/carina_nonexistent_path_xyz");
        let base = get_base_dir(path);
        assert_eq!(base, path);
    }

    // ========== load_configuration tests ==========

    #[test]
    fn load_configuration_single_file() {
        let dir = create_temp_dir("load_single_file");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&file).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_single_file_with_backend() {
        let dir = create_temp_dir("load_single_backend");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&file).unwrap();
        assert!(config.parsed.backend.is_some());
        assert_eq!(config.backend_file.unwrap(), file);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_nonexistent_path() {
        let path = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_file_xyz");
        let result = load_configuration(&path);
        match result {
            Err(e) => assert!(e.contains("Path not found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for nonexistent path"),
        }
    }

    #[test]
    fn load_configuration_empty_directory() {
        let dir = create_temp_dir("load_empty_dir");
        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(e.contains("No .crn files found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for empty directory"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_single_file() {
        let dir = create_temp_dir("load_dir_single");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_merges_multiple_files() {
        let dir = create_temp_dir("load_dir_merge");
        fs::write(
            dir.join("provider.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("backend.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_ignores_non_crn_files() {
        let dir = create_temp_dir("load_dir_ignore_non_crn");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "not a crn file").unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_parse_error() {
        let dir = create_temp_dir("load_dir_parse_error");
        fs::write(dir.join("bad.crn"), "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&dir);
        assert!(result.is_err());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_single_file_with_parse_error() {
        let dir = create_temp_dir("load_file_parse_error");
        let file = dir.join("bad.crn");
        fs::write(&file, "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(e.contains("Parse error"), "unexpected error: {}", e),
            Ok(_) => panic!("expected parse error"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_preserves_unresolved_parsed() {
        let dir = create_temp_dir("load_unresolved");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&file).unwrap();
        // unresolved_parsed should also have the provider
        assert_eq!(config.unresolved_parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_multiple_backends_error() {
        let dir = create_temp_dir("load_dir_multi_backend");
        fs::write(
            dir.join("a.crn"),
            r#"backend s3 {
    bucket = "bucket-a"
    key    = "state-a.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("b.crn"),
            r#"backend s3 {
    bucket = "bucket-b"
    key    = "state-b.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(
                e.contains("multiple backend blocks defined"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for multiple backends"),
        }
        cleanup(&dir);
    }
}
