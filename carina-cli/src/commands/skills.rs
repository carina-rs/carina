use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

/// The embedded SKILL.md content, baked in at compile time.
pub const EMBEDDED_SKILL: &str = include_str!("../../../skills/carina/SKILL.md");

/// The skill directory name (must match the `name` field in SKILL.md frontmatter).
const SKILL_NAME: &str = "carina";

/// Return the install directory: `~/.agents/skills/carina/`
fn install_dir() -> Result<PathBuf, AppError> {
    let home = std::env::var("HOME")
        .map_err(|_| AppError::Config("Could not determine home directory".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".agents")
        .join("skills")
        .join(SKILL_NAME))
}

/// Extract the `version` value from `metadata:` in SKILL.md frontmatter.
fn extract_version(content: &str) -> Option<&str> {
    let mut in_frontmatter = false;
    let mut in_metadata = false;
    for line in content.lines() {
        if line.trim() == "---" {
            if in_frontmatter {
                return None; // end of frontmatter, version not found
            }
            in_frontmatter = true;
            continue;
        }
        if !in_frontmatter {
            continue;
        }
        if line.starts_with("metadata:") {
            in_metadata = true;
            continue;
        }
        if in_metadata {
            // metadata fields are indented
            if !line.starts_with(' ') && !line.starts_with('\t') {
                in_metadata = false;
                continue;
            }
            if let Some(rest) = line.trim().strip_prefix("version:") {
                let v = rest.trim().trim_matches('"');
                return Some(v);
            }
        }
    }
    None
}

// --- Path-parameterized core functions (shared by production + tests) ---

fn install_to(dir: &Path) -> Result<String, AppError> {
    let path = dir.join("SKILL.md");
    fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {}", dir.display(), e))?;
    fs::write(&path, EMBEDDED_SKILL)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(format!("Installed skill to {}", path.display()))
}

fn uninstall_from(dir: &Path) -> Result<String, AppError> {
    if !dir.exists() {
        return Ok("No skills installed.".to_string());
    }
    fs::remove_dir_all(dir).map_err(|e| format!("Failed to remove {}: {}", dir.display(), e))?;
    Ok(format!("Uninstalled skill from {}", dir.display()))
}

fn status_of(skill_path: &Path) -> Result<String, AppError> {
    let embedded_version = extract_version(EMBEDDED_SKILL).unwrap_or("unknown");

    if !skill_path.exists() {
        return Ok(format!(
            "Not installed.\n  Embedded version: v{}\n  Run 'carina skills install' to install.",
            embedded_version
        ));
    }

    let installed_content = fs::read_to_string(skill_path)
        .map_err(|e| format!("Failed to read {}: {}", skill_path.display(), e))?;
    let installed_version = extract_version(&installed_content).unwrap_or("unknown");

    if embedded_version == installed_version {
        Ok(format!(
            "Installed at: {}\n  Version: v{} (up to date)",
            skill_path.display(),
            installed_version
        ))
    } else {
        Ok(format!(
            "Installed at: {}\n  Installed version: v{}\n  Embedded version: v{}\n  Run 'carina skills update' to update.",
            skill_path.display(),
            installed_version,
            embedded_version
        ))
    }
}

fn update_to(dir: &Path) -> Result<String, AppError> {
    let skill_path = dir.join("SKILL.md");
    let embedded_version = extract_version(EMBEDDED_SKILL).unwrap_or("unknown");

    if !skill_path.exists() {
        return install_to(dir);
    }

    let installed_content = fs::read_to_string(&skill_path)
        .map_err(|e| format!("Failed to read {}: {}", skill_path.display(), e))?;
    let installed_version = extract_version(&installed_content).unwrap_or("unknown");

    if embedded_version == installed_version {
        return Ok(format!("Already up to date (v{}).", installed_version));
    }

    fs::write(&skill_path, EMBEDDED_SKILL)
        .map_err(|e| format!("Failed to write {}: {}", skill_path.display(), e))?;
    Ok(format!(
        "Updated from v{} to v{} at {}",
        installed_version,
        embedded_version,
        skill_path.display()
    ))
}

// --- Public API (thin wrappers using default install path) ---

/// `carina skills list` — list embedded skills.
pub fn run_skills_list() -> String {
    let version = extract_version(EMBEDDED_SKILL).unwrap_or("unknown");
    format!("Embedded skills:\n\n  {:<20} v{}", SKILL_NAME, version)
}

/// `carina skills install` — install SKILL.md to `~/.agents/skills/carina/`.
pub fn run_skills_install() -> Result<String, AppError> {
    install_to(&install_dir()?)
}

/// `carina skills uninstall` — remove managed skills.
pub fn run_skills_uninstall() -> Result<String, AppError> {
    uninstall_from(&install_dir()?)
}

/// `carina skills status` — show install status and version comparison.
pub fn run_skills_status() -> Result<String, AppError> {
    status_of(&install_dir()?.join("SKILL.md"))
}

/// `carina skills update` — update installed skill if versions differ.
pub fn run_skills_update() -> Result<String, AppError> {
    update_to(&install_dir()?)
}

/// `carina skills reinstall` — force reinstall regardless of version.
pub fn run_skills_reinstall() -> Result<String, AppError> {
    install_to(&install_dir()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_embedded_skill_is_not_empty() {
        assert!(!EMBEDDED_SKILL.is_empty());
    }

    #[test]
    fn test_embedded_skill_has_frontmatter() {
        assert!(EMBEDDED_SKILL.starts_with("---\n"));
        let rest = &EMBEDDED_SKILL[4..];
        assert!(rest.contains("\n---\n"));
    }

    #[test]
    fn test_embedded_skill_has_name_field() {
        assert!(EMBEDDED_SKILL.contains("name: carina"));
    }

    #[test]
    fn test_extract_version() {
        assert_eq!(extract_version(EMBEDDED_SKILL), Some("0.3.0"));
    }

    #[test]
    fn test_extract_version_missing() {
        let content = "---\nname: test\n---\nHello";
        assert_eq!(extract_version(content), None);
    }

    #[test]
    fn test_extract_version_ignores_top_level_version() {
        let content = "---\nversion: \"9.9.9\"\nmetadata:\n  version: \"1.2.3\"\n---\nBody";
        assert_eq!(extract_version(content), Some("1.2.3"));
    }

    #[test]
    fn test_run_skills_list() {
        let output = run_skills_list();
        assert!(output.contains("carina"));
        assert!(output.contains("v0.3.0"));
    }

    #[test]
    fn test_install_to_creates_skill_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        let result = install_to(&dir);
        assert!(result.is_ok());
        let path = dir.join("SKILL.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, EMBEDDED_SKILL);
    }

    #[test]
    fn test_status_not_installed() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("carina").join("SKILL.md");
        let result = status_of(&path).unwrap();
        assert!(result.contains("Not installed"));
    }

    #[test]
    fn test_status_up_to_date() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        install_to(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let result = status_of(&path).unwrap();
        assert!(result.contains("up to date"));
    }

    #[test]
    fn test_status_version_mismatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let old_content = EMBEDDED_SKILL.replace("version: \"0.3.0\"", "version: \"0.1.0\"");
        fs::write(&path, &old_content).unwrap();
        let result = status_of(&path).unwrap();
        assert!(result.contains("v0.1.0"));
        assert!(result.contains("v0.3.0"));
        assert!(result.contains("update"));
    }

    #[test]
    fn test_update_installs_when_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        let result = update_to(&dir).unwrap();
        assert!(result.contains("Installed"));
        assert!(dir.join("SKILL.md").exists());
    }

    #[test]
    fn test_update_no_op_when_current() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        install_to(&dir).unwrap();
        let result = update_to(&dir).unwrap();
        assert!(result.contains("up to date"));
    }

    #[test]
    fn test_update_updates_old_version() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        let old_content = EMBEDDED_SKILL.replace("version: \"0.3.0\"", "version: \"0.1.0\"");
        fs::write(&path, &old_content).unwrap();
        let result = update_to(&dir).unwrap();
        assert!(result.contains("Updated from v0.1.0 to v0.3.0"));
        let new_content = fs::read_to_string(&path).unwrap();
        assert_eq!(new_content, EMBEDDED_SKILL);
    }

    #[test]
    fn test_uninstall_from_removes_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        install_to(&dir).unwrap();
        assert!(dir.exists());
        let result = uninstall_from(&dir).unwrap();
        assert!(result.contains("Uninstalled"));
        assert!(!dir.exists());
    }

    #[test]
    fn test_uninstall_from_not_installed() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("carina");
        let result = uninstall_from(&dir).unwrap();
        assert!(result.contains("No skills installed"));
    }
}
