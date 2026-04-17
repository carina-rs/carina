use std::fs;
use std::path::PathBuf;

use colored::Colorize;
use serde::Serialize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::find_duplicate_attrs;
use carina_core::parser::{ProviderContext, UpstreamState};

use super::validate_and_resolve_with_config;
use crate::error::AppError;
use crate::wiring::check_unused_bindings;

#[derive(Serialize)]
struct ValidateOutput {
    status: &'static str,
    resource_count: usize,
    resources: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<ValidateWarning>,
}

#[derive(Serialize)]
struct ValidateWarning {
    #[serde(rename = "type")]
    warning_type: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

pub fn run_validate(
    path: &PathBuf,
    json: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(path, provider_context)?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);

    // Surface bad `upstream_state.source` paths before printing warnings —
    // otherwise the "is not yet in the upstream state" warning implies the
    // source is reachable and misdirects the user.
    check_upstream_state_sources(base_dir, &parsed.upstream_states)?;

    parsed.print_warnings();

    if !json {
        println!("{}", "Validating...".cyan());
    }

    validate_and_resolve_with_config(&mut parsed, base_dir, false)?;

    // Check for unused let bindings (warnings, not errors)
    let unused_warnings = check_unused_bindings(&loaded.unresolved_parsed);

    // Check for duplicate attribute keys
    let source_files: Vec<(PathBuf, String)> = {
        let files = find_crn_files_in_dir(path)?;
        let mut texts = Vec::new();
        for file in files {
            let content = fs::read_to_string(&file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            texts.push((file, content));
        }
        texts
    };

    let mut duplicate_warnings: Vec<(PathBuf, String)> = Vec::new();
    for (file_path, source) in &source_files {
        for dup in find_duplicate_attrs(source) {
            duplicate_warnings.push((
                file_path.clone(),
                format!(
                    "Duplicate attribute '{}' at line {} (first defined on line {}). The last value will be used.",
                    dup.name, dup.line, dup.first_line
                ),
            ));
        }
    }

    if json {
        let mut warnings = Vec::new();
        for binding in &unused_warnings {
            warnings.push(ValidateWarning {
                warning_type: "unused_binding",
                message: format!("Unused let binding '{}'", binding),
                file: None,
            });
        }
        for (file_path, message) in &duplicate_warnings {
            warnings.push(ValidateWarning {
                warning_type: "duplicate_attribute",
                message: message.clone(),
                file: Some(file_path.display().to_string()),
            });
        }
        let output = ValidateOutput {
            status: "ok",
            resource_count: parsed.resources.len(),
            resources: parsed.resources.iter().map(|r| r.id.to_string()).collect(),
            warnings,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|e| format!("Failed to serialize: {}", e))?
        );
        return Ok(());
    }

    println!(
        "{}",
        format!(
            "✓ {} resources validated successfully.",
            parsed.resources.len()
        )
        .green()
        .bold()
    );

    for resource in &parsed.resources {
        println!("  • {}", resource.id);
    }

    for binding in &unused_warnings {
        println!(
            "{}",
            format!(
                "⚠ Unused let binding '{}'. Consider using an anonymous resource instead.",
                binding
            )
            .yellow()
        );
    }

    for (file_path, message) in &duplicate_warnings {
        println!(
            "{}",
            format!("⚠ {}:{}", file_path.display(), message).yellow()
        );
    }

    Ok(())
}

/// Verify that every `upstream_state.source` resolves to an existing directory.
///
/// Cheaper than plan-time `load_upstream_states` (no canonicalize, no backend
/// I/O) — this is a lightweight early signal run during `validate`. All
/// failures are accumulated so the user sees every bad path at once.
fn check_upstream_state_sources(
    base_dir: &std::path::Path,
    upstream_states: &[UpstreamState],
) -> Result<(), AppError> {
    let mut errors = Vec::new();
    for us in upstream_states {
        if !base_dir.join(&us.source).is_dir() {
            errors.push(format!(
                "upstream_state '{}': source '{}' does not exist",
                us.binding,
                us.source.display()
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Validation(errors.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_output_serialization() {
        let output = ValidateOutput {
            status: "ok",
            resource_count: 2,
            resources: vec![
                "aws.s3.bucket.my-bucket".to_string(),
                "aws.ec2.vpc.main".to_string(),
            ],
            warnings: vec![],
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["resource_count"], 2);
        assert_eq!(parsed["resources"].as_array().unwrap().len(), 2);
        // warnings should be omitted when empty
        assert!(parsed.get("warnings").is_none());
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: binding.to_string(),
            source: std::path::PathBuf::from(source),
        }
    }

    #[test]
    fn check_upstream_state_sources_accepts_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("project");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(tmp.path().join("upstream")).unwrap();

        check_upstream_state_sources(&base, &[upstream("orgs", "../upstream")]).unwrap();
    }

    #[test]
    fn check_upstream_state_sources_rejects_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("project");
        std::fs::create_dir(&base).unwrap();

        let err = check_upstream_state_sources(&base, &[upstream("orgs", "../nonexistent")])
            .expect_err("missing source should error");
        let msg = err.to_string();
        assert!(
            msg.contains("upstream_state 'orgs'") && msg.contains("../nonexistent"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn check_upstream_state_sources_reports_every_missing_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("project");
        std::fs::create_dir(&base).unwrap();

        let err = check_upstream_state_sources(
            &base,
            &[upstream("a", "../missing_a"), upstream("b", "../missing_b")],
        )
        .expect_err("missing sources should error");
        let msg = err.to_string();
        assert!(msg.contains("upstream_state 'a'"), "missing 'a': {msg}");
        assert!(msg.contains("upstream_state 'b'"), "missing 'b': {msg}");
    }

    #[test]
    fn test_validate_output_with_warnings() {
        let output = ValidateOutput {
            status: "ok",
            resource_count: 1,
            resources: vec!["aws.s3.bucket.test".to_string()],
            warnings: vec![
                ValidateWarning {
                    warning_type: "unused_binding",
                    message: "Unused let binding 'temp'".to_string(),
                    file: None,
                },
                ValidateWarning {
                    warning_type: "duplicate_attribute",
                    message: "Duplicate attribute 'tags'".to_string(),
                    file: Some("main.crn".to_string()),
                },
            ],
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["warnings"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["warnings"][0]["type"], "unused_binding");
        assert!(parsed["warnings"][0].get("file").is_none());
        assert_eq!(parsed["warnings"][1]["file"], "main.crn");
    }
}
