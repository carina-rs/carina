use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::find_duplicate_attrs;
use carina_core::parser::ProviderContext;

use super::validate_and_resolve_with_config;
use crate::error::AppError;
use crate::wiring::check_unused_bindings;

pub fn run_validate(path: &PathBuf, provider_context: &ProviderContext) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(path, provider_context)?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);

    println!("{}", "Validating...".cyan());

    validate_and_resolve_with_config(&mut parsed, base_dir, false, provider_context)?;

    // Check for unused let bindings (warnings, not errors)
    // Use unresolved_parsed because resolve_resource_refs resolves intermediate
    // ResourceRef values away (e.g., igw_attachment.id -> igw.id), making
    // intermediate bindings appear unused even though they are structurally needed.
    let unused_warnings = check_unused_bindings(&loaded.unresolved_parsed);

    // Check for duplicate attribute keys
    let source_files: Vec<(PathBuf, String)> = if path.is_file() {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        vec![(path.clone(), content)]
    } else if path.is_dir() {
        let files = find_crn_files_in_dir(path)?;
        let mut texts = Vec::new();
        for file in files {
            let content = fs::read_to_string(&file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            texts.push((file, content));
        }
        texts
    } else {
        vec![]
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
