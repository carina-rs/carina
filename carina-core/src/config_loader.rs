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
            inputs: vec![],
            outputs: vec![],
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
                    unresolved_merged.inputs.extend(unresolved.inputs);
                    unresolved_merged.outputs.extend(unresolved.outputs);

                    // Merge resolved
                    merged.providers.extend(parsed.providers);
                    merged.resources.extend(parsed.resources);
                    merged.variables.extend(parsed.variables);
                    merged.imports.extend(parsed.imports);
                    merged.module_calls.extend(parsed.module_calls);
                    merged.inputs.extend(parsed.inputs);
                    merged.outputs.extend(parsed.outputs);
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
