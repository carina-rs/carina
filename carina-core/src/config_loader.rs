//! Configuration loading and .crn file discovery utilities

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::parser::{self, ParsedFile, ProviderContext};

/// Result of loading configuration, includes the file path containing backend block.
///
/// The struct holds non-blocking diagnostics (e.g. deferred for-iterable
/// binding errors) so the caller can decide to keep running the rest of
/// the static-analysis pipeline and report them together with downstream
/// findings (#2102). Blocking errors (bad parse, missing modules) still
/// come back as `Err` from the loader.
pub struct LoadedConfig {
    pub parsed: ParsedFile,
    /// Resources before reference resolution, for unused binding detection.
    /// After `resolve_resource_refs`, intermediate `ResourceRef` values are resolved away,
    /// so this preserves the original references for accurate unused binding analysis.
    pub unresolved_parsed: ParsedFile,
    pub backend_file: Option<PathBuf>,
    /// Deferred for-iterables whose binding didn't resolve against the
    /// directory-wide merge. Empty when every iterable is in scope. The
    /// caller is responsible for surfacing these — the loader does not
    /// short-circuit on them so that later validators can also run and
    /// their findings can be reported alongside.
    pub iterable_binding_errors: Vec<parser::ParseError>,
}

/// Load configuration from a directory containing .crn files
pub fn load_configuration(path: &PathBuf) -> Result<LoadedConfig, String> {
    load_configuration_with_config(path, &ProviderContext::default())
}

/// Load configuration from a directory containing .crn files.
///
/// File paths are rejected — pass a directory instead.
pub fn load_configuration_with_config(
    path: &PathBuf,
    config: &ProviderContext,
) -> Result<LoadedConfig, String> {
    if path.is_file() {
        Err(format!("expected directory, got file: {}", path.display()))
    } else if path.is_dir() {
        // Directory mode
        let files = find_crn_files_in_dir(path)?;
        if files.is_empty() {
            return Err(format!("No .crn files found in {}", path.display()));
        }

        let empty_parsed = ParsedFile::default;
        let mut merged = empty_parsed();
        let mut unresolved_merged = empty_parsed();
        let mut parse_errors = Vec::new();
        let mut backend_file: Option<PathBuf> = None;

        for file in &files {
            let content = fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            match parser::parse(&content, config) {
                Ok(mut parsed) => {
                    // Stamp the full source path onto warnings and deferred
                    // for-expressions. Bare filenames are ambiguous when a
                    // project spans multiple `.crn` files (and identically
                    // named files in sibling directories), and they break
                    // editor jump-to-location.
                    let file_path = Some(file.display().to_string());
                    for w in &mut parsed.warnings {
                        w.file = file_path.clone();
                    }
                    for d in &mut parsed.deferred_for_expressions {
                        d.file = file_path.clone();
                    }

                    let unresolved = parsed.clone();
                    if let Err(e) = parser::resolve_resource_refs_with_config(&mut parsed, config) {
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
                    unresolved_merged
                        .export_params
                        .extend(unresolved.export_params);
                    unresolved_merged
                        .state_blocks
                        .extend(unresolved.state_blocks);
                    unresolved_merged
                        .user_functions
                        .extend(unresolved.user_functions);
                    unresolved_merged
                        .upstream_states
                        .extend(unresolved.upstream_states);
                    unresolved_merged
                        .structural_bindings
                        .extend(unresolved.structural_bindings);
                    unresolved_merged.warnings.extend(unresolved.warnings);
                    unresolved_merged
                        .deferred_for_expressions
                        .extend(unresolved.deferred_for_expressions);

                    // Merge resolved
                    merged.providers.extend(parsed.providers);
                    merged.resources.extend(parsed.resources);
                    merged.variables.extend(parsed.variables);
                    merged.imports.extend(parsed.imports);
                    merged.module_calls.extend(parsed.module_calls);
                    merged.arguments.extend(parsed.arguments);
                    merged.attribute_params.extend(parsed.attribute_params);
                    merged.export_params.extend(parsed.export_params);
                    merged.state_blocks.extend(parsed.state_blocks);
                    merged.user_functions.extend(parsed.user_functions);
                    merged.upstream_states.extend(parsed.upstream_states);
                    merged.requires.extend(parsed.requires);
                    merged
                        .structural_bindings
                        .extend(parsed.structural_bindings);
                    merged.warnings.extend(parsed.warnings);
                    merged
                        .deferred_for_expressions
                        .extend(parsed.deferred_for_expressions);
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

        // Resolve cross-file forward references on the merged result.
        // Per-file resolve_resource_refs_with_config (line 78) only sees
        // bindings within each file; cross-file dot-notation strings in
        // export_params (e.g., "registry_prod.account_id") remain as
        // Value::String. This second pass converts them to ResourceRef.
        if let Err(e) = parser::resolve_resource_refs_with_config(&mut merged, config) {
            return Err(e.to_string());
        }

        // Deferred for-iterable binding errors are accumulated rather than
        // short-circuited so `carina validate` can keep going and report
        // every static error in one pass (#2102). The caller reads
        // `iterable_binding_errors` and merges them with its own findings.
        let iterable_binding_errors = parser::check_deferred_for_iterables(&merged);

        Ok(LoadedConfig {
            parsed: merged,
            unresolved_parsed: unresolved_merged,
            backend_file,
            iterable_binding_errors,
        })
    } else {
        Err(format!("Path not found: {}", path.display()))
    }
}

/// Parse all `.crn` files in a directory and return a merged `ParsedFile`.
///
/// This is the canonical directory-scope parse: each file is parsed
/// individually, results are merged, and cross-file references are
/// resolved against the combined binding map. Both CLI and LSP should
/// use this to ensure consistent results.
///
/// Returns `None` for `export_params` values that are cross-file string
/// references (e.g. `"registry_prod.account_id"`) resolved to `ResourceRef`.
pub fn parse_directory(dir: &Path, config: &ProviderContext) -> Result<ParsedFile, String> {
    parse_directory_with_overrides(dir, config, &HashMap::new())
}

/// Like [`parse_directory`] but takes an `overrides` map from **file name**
/// (e.g. `"main.crn"`) to source text. Files present in the map are parsed
/// from the override text; the rest are read from disk.
///
/// LSP uses this to analyze the open document's in-memory buffer alongside
/// the on-disk siblings, so edits show diagnostics before the user saves.
pub fn parse_directory_with_overrides(
    dir: &Path,
    config: &ProviderContext,
    overrides: &HashMap<String, String>,
) -> Result<ParsedFile, String> {
    let dir_buf = dir.to_path_buf();
    let files = find_crn_files_in_dir(&dir_buf)?;
    let mut paths: Vec<(std::path::PathBuf, String)> = files
        .into_iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str().map(|s| (p.clone(), s.to_string())))
        })
        .collect();
    // Also include overrides whose filename isn't on disk yet (e.g. a new
    // buffer the user hasn't saved). Keep the list de-duplicated by name.
    for name in overrides.keys() {
        if !paths.iter().any(|(_, n)| n == name) {
            paths.push((dir_buf.join(name), name.clone()));
        }
    }
    if paths.is_empty() {
        return Err(format!("No .crn files found in {}", dir.display()));
    }

    let mut merged = ParsedFile::default();
    let mut parse_errors = Vec::new();

    for (file, name) in &paths {
        let content = match overrides.get(name) {
            Some(buffer) => buffer.clone(),
            None => fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?,
        };
        match parser::parse(&content, config) {
            Ok(mut parsed) => {
                let file_path = Some(file.display().to_string());
                for w in &mut parsed.warnings {
                    w.file = file_path.clone();
                }
                for d in &mut parsed.deferred_for_expressions {
                    d.file = file_path.clone();
                }
                merge_parsed_file(&mut merged, parsed);
            }
            Err(e) => {
                parse_errors.push(format!("{}: {}", file.display(), e));
            }
        }
    }

    if !parse_errors.is_empty() {
        return Err(parse_errors.join("\n"));
    }

    // Resolve cross-file references on the merged result
    if let Err(e) = parser::resolve_resource_refs_with_config(&mut merged, config) {
        return Err(e.to_string());
    }

    // Deferred-for-iterable validation is left to callers. LSP needs the
    // ParsedFile even when an iterable names an undefined binding, so it
    // can surface the error as a diagnostic; CLI entry points run
    // `check_deferred_for_iterables` themselves.
    Ok(merged)
}

/// Merge fields from `source` into `target`.
pub(crate) fn merge_parsed_file(target: &mut ParsedFile, source: ParsedFile) {
    target.providers.extend(source.providers);
    target.resources.extend(source.resources);
    target.variables.extend(source.variables);
    target.imports.extend(source.imports);
    target.module_calls.extend(source.module_calls);
    target.arguments.extend(source.arguments);
    target.attribute_params.extend(source.attribute_params);
    target.export_params.extend(source.export_params);
    target.state_blocks.extend(source.state_blocks);
    target.user_functions.extend(source.user_functions);
    target.upstream_states.extend(source.upstream_states);
    target.requires.extend(source.requires);
    target
        .structural_bindings
        .extend(source.structural_bindings);
    target.warnings.extend(source.warnings);
    target
        .deferred_for_expressions
        .extend(source.deferred_for_expressions);
    if let Some(backend) = source.backend {
        target.backend = Some(backend);
    }
}

/// Get base directory for module resolution.
///
/// Since paths are now always directories, this returns the path as-is.
/// Kept for backward compatibility with callers.
pub fn get_base_dir(path: &Path) -> &Path {
    path
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
    fn get_base_dir_returns_path_as_is() {
        let dir = create_temp_dir("base_dir_as_is");

        let base = get_base_dir(&dir);
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
    fn load_configuration_directory_with_provider() {
        let dir = create_temp_dir("load_dir_provider");
        fs::write(
            dir.join("test.crn"),
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
    fn load_configuration_directory_with_backend() {
        let dir = create_temp_dir("load_dir_backend");
        fs::write(
            dir.join("test.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
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
    fn load_configuration_rejects_file_path() {
        let dir = create_temp_dir("load_rejects_file");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_file_path_rejected_even_with_bad_content() {
        let dir = create_temp_dir("load_file_parse_error");
        let file = dir.join("bad.crn");
        fs::write(&file, "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_stamps_full_path_on_warnings() {
        // Issue #1997: warnings and deferred for-expressions must carry a full
        // source path, not a bare filename. Bare filenames are ambiguous when a
        // project spans multiple .crn files and break editor jump-to-location.
        let dir = create_temp_dir("load_warning_full_path");
        // A for-loop with an unused binding generates a ParseWarning.
        let crn_path = dir.join("main.crn");
        fs::write(
            &crn_path,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let empty_list = []
let _ = for unused_var in empty_list {
    aws.ec2.vpc {
        cidr_block = "10.0.0.0/16"
    }
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(
            !config.parsed.warnings.is_empty(),
            "fixture must produce at least one ParseWarning"
        );
        let stamped = config.parsed.warnings[0]
            .file
            .as_ref()
            .expect("warning must have a file path stamped");
        assert_eq!(
            stamped,
            &crn_path.display().to_string(),
            "warning.file must carry the full source path, not a bare filename",
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_preserves_unresolved_parsed() {
        let dir = create_temp_dir("load_unresolved");
        fs::write(
            dir.join("test.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
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

    #[test]
    fn parse_directory_accepts_cross_file_upstream_state_in_for_expression() {
        let dir = create_temp_dir("cross_file_upstream_for");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        assert!(
            result.is_ok(),
            "expected cross-file upstream_state binding in `for` to resolve, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn cross_file_upstream_state_refs_emit_no_soft_warning() {
        // Field validity is checked statically by the `upstream_exports`
        // module now. Loader- and parser-level "validate does not inspect"
        // soft warnings are gone across the board.
        let dir = create_temp_dir("cross_file_upstream_no_warning");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}

let network = upstream_state {
  source = '../network'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.bucket {
    name = name
  }
}

awscc.ec2.security_group {
  group_description = 'Web SG'
  vpc_id = network.vpc_id
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        let loaded = result.expect("load should succeed");
        cleanup(&dir);

        let upstream_warnings: Vec<_> = loaded
            .parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("upstream_state"))
            .collect();
        assert!(
            upstream_warnings.is_empty(),
            "loader should emit no soft upstream_state warnings, got: {:?}",
            upstream_warnings
        );
    }

    #[test]
    fn parse_directory_leaves_undefined_iterable_for_caller_to_check() {
        let dir = create_temp_dir("undefined_for_iterable");
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  aws.s3.bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        let parsed = result.expect("parse_directory should not fail on undefined iterable");
        let errs = parser::check_deferred_for_iterables(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        match &errs[0] {
            parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected error: {other}"),
        }

        // `load_configuration_with_config` surfaces the error via
        // `LoadedConfig::iterable_binding_errors` rather than short-
        // circuiting, so the CLI caller can collect it together with
        // findings from later validators in a single pass (#2102).
        let dir2 = create_temp_dir("undefined_for_iterable_cli");
        fs::write(
            dir2.join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  aws.s3.bucket {
    name = name
  }
}
"#,
        )
        .unwrap();
        let loaded = load_configuration_with_config(&dir2, &ProviderContext::default())
            .expect("load_configuration should not short-circuit on iterable binding errors");
        cleanup(&dir2);
        assert_eq!(
            loaded.iterable_binding_errors.len(),
            1,
            "expected one iterable binding error, got {:?}",
            loaded.iterable_binding_errors,
        );
        match &loaded.iterable_binding_errors[0] {
            parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
