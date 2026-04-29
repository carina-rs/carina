//! Filesystem loading helpers for modules.
//!
//! Modules are directory-scoped: every helper here treats a module as a
//! directory containing one or more `.crn` files, never as a single file.

use std::fs;
use std::path::{Path, PathBuf};

use crate::parser::{ParsedFile, ProviderContext};

use super::error::ModuleError;

/// Get parsed file info for display (supports both module definitions and root configs)
pub fn get_parsed_file(path: &Path) -> Result<ParsedFile, ModuleError> {
    let content = fs::read_to_string(path)?;
    let parsed = crate::parser::parse(&content, &ProviderContext::default())?;
    Ok(parsed)
}

/// Load a module from a directory path.
///
/// Modules are directory-scoped: all `.crn` files in the directory are merged
/// uniformly, with no file name (including `main.crn`) treated as privileged.
///
/// Returns `None` if `path` is not a directory, cannot be read/parsed, or
/// contains no module definitions (no inputs or outputs).
pub fn load_module(path: &Path) -> Option<ParsedFile> {
    if !path.is_dir() {
        return None;
    }
    load_directory_module(path)
}

/// Collect `.crn` file paths directly inside `dir_path`, sorted by path.
///
/// Sorting is load-bearing: merged `ParsedFile` vectors inherit this order,
/// and downstream consumers (LSP first-match-wins lookups, CLI diagnostic
/// ordering) must not depend on filesystem iteration order, which varies
/// across ext4/APFS/tmpfs.
pub(super) fn sorted_crn_paths_in(dir_path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut crn_files: Vec<PathBuf> = fs::read_dir(dir_path)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "crn"))
        .collect();
    crn_files.sort();
    Ok(crn_files)
}

/// Load all `.crn` files from a directory and merge them into a single `ParsedFile`.
///
/// Returns `None` if the directory cannot be read or contains no module
/// definitions (no arguments/attributes).
pub fn load_directory_module(dir_path: &Path) -> Option<ParsedFile> {
    let mut merged = ParsedFile::default();

    for path in sorted_crn_paths_in(dir_path).ok()? {
        if let Ok(content) = fs::read_to_string(&path)
            && let Ok(parsed) = crate::parser::parse(&content, &ProviderContext::default())
        {
            crate::config_loader::merge_parsed_file(&mut merged, parsed);
        }
    }

    if merged.arguments.is_empty() && merged.attribute_params.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Derive the module name from a file or directory path.
///
/// Examples:
/// - `modules/web_tier/` → `web_tier` (directory)
/// - `modules/web_tier/main.crn` → `web_tier` (directory-based)
/// - `modules/web_tier.crn` → `web_tier` (file-based)
/// - `web_tier.crn` → `web_tier`
pub fn derive_module_name(path: &Path) -> String {
    if path.is_dir() {
        return path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
    }

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // If file is named main.crn, use the parent directory name
    if file_stem == "main"
        && let Some(parent) = path.parent()
        && let Some(parent_name) = parent.file_name()
        && let Some(name) = parent_name.to_str()
    {
        return name.to_string();
    }

    file_stem.to_string()
}

/// Load a module from a directory by reading all `.crn` files.
///
/// Unlike [`load_directory_module`], this returns a `Result` with descriptive
/// error messages and does not check for module definitions (inputs/outputs).
pub fn load_module_from_directory(dir: &Path) -> Result<ParsedFile, String> {
    let paths = sorted_crn_paths_in(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut merged = ParsedFile::default();
    for path in paths {
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let parsed = crate::parser::parse(&content, &ProviderContext::default())
            .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
        crate::config_loader::merge_parsed_file(&mut merged, parsed);
    }

    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Loader is directory-scoped. Multi-file fixture asserts that sibling
    /// `.crn` files in the same module directory are merged into the
    /// returned `ParsedFile` — never read in isolation.
    #[test]
    fn load_module_merges_sibling_crn_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("main.crn"), "# main module file\n").unwrap();
        fs::write(dir.join("arguments.crn"), "arguments {\n  env: String\n}\n").unwrap();
        fs::write(
            dir.join("exports.crn"),
            "exports {\n  region = \"ap-northeast-1\"\n}\n",
        )
        .unwrap();

        let parsed = load_module(dir).expect("module should load when arguments are declared");
        assert_eq!(parsed.arguments.len(), 1);
        assert_eq!(parsed.arguments[0].name, "env");
        assert_eq!(parsed.export_params.len(), 1);
        assert_eq!(parsed.export_params[0].name, "region");
    }

    /// Sibling `.crn` files must be merged in sorted filename order so that
    /// downstream first-match-wins lookups (LSP hover/completion, CLI
    /// diagnostic ordering) are deterministic across filesystems.
    #[test]
    fn load_module_directory_merge_order_is_deterministic() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("z_last.crn"), "arguments {\n  c: String\n}\n").unwrap();
        fs::write(dir.join("a_first.crn"), "arguments {\n  a: String\n}\n").unwrap();
        fs::write(dir.join("m_middle.crn"), "arguments {\n  b: String\n}\n").unwrap();

        let parsed = load_module(dir).expect("module should load");
        let names: Vec<&str> = parsed.arguments.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn load_module_returns_none_for_file_path() {
        let tmp = tempdir().unwrap();
        let single = tmp.path().join("solo.crn");
        fs::write(&single, "arguments {\n  x: String\n}\n").unwrap();
        assert!(load_module(&single).is_none());
    }

    #[test]
    fn load_module_from_directory_merges_multiple_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();

        fs::write(dir.join("a.crn"), "arguments {\n  a: String\n}\n").unwrap();
        fs::write(dir.join("b.crn"), "arguments {\n  b: String\n}\n").unwrap();

        let parsed = load_module_from_directory(dir).expect("dir should load");
        assert_eq!(parsed.arguments.len(), 2);
    }

    #[test]
    fn derive_module_name_uses_directory_name_for_directory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("web_tier");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(derive_module_name(&dir), "web_tier");
    }

    #[test]
    fn derive_module_name_uses_parent_for_main_crn() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("web_tier");
        fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main.crn");
        fs::write(&main, "").unwrap();
        assert_eq!(derive_module_name(&main), "web_tier");
    }

    #[test]
    fn derive_module_name_uses_file_stem_for_other_files() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("custom.crn");
        fs::write(&path, "").unwrap();
        assert_eq!(derive_module_name(&path), "custom");
    }
}
