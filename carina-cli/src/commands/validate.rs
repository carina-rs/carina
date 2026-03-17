use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration};

use super::validate_and_resolve;
use crate::error::AppError;
use crate::wiring::check_unused_bindings;

pub fn run_validate(path: &PathBuf) -> Result<(), AppError> {
    let loaded = load_configuration(path)?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);

    println!("{}", "Validating...".cyan());

    validate_and_resolve(&mut parsed, base_dir, false)?;

    // Check for unused let bindings (warnings, not errors)
    // Use unresolved_parsed because resolve_resource_refs resolves intermediate
    // ResourceRef values away (e.g., igw_attachment.id -> igw.id), making
    // intermediate bindings appear unused even though they are structurally needed.
    let unused_warnings = check_unused_bindings(&loaded.unresolved_parsed);

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

    Ok(())
}
