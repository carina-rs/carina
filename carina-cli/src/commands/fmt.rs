use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::config_loader::{find_crn_files_in_dir, find_crn_files_recursive};
use carina_core::formatter::{self, FormatConfig};
use carina_core::schema::collect_all_block_names;

use crate::error::AppError;
use crate::wiring::WiringContext;

pub fn run_fmt(
    path: &PathBuf,
    check: bool,
    show_diff: bool,
    recursive: bool,
) -> Result<(), AppError> {
    let config = FormatConfig::default();

    // Load schemas to get block_name mappings for list-to-block conversion
    let ctx = WiringContext::new();
    let block_names = collect_all_block_names(ctx.schemas());

    let files = if path.is_file() {
        vec![path.clone()]
    } else if recursive {
        find_crn_files_recursive(path)?
    } else {
        find_crn_files_in_dir(path)?
    };

    if files.is_empty() {
        println!("{}", "No .crn files found.".yellow());
        return Ok(());
    }

    let mut needs_formatting = Vec::new();
    let mut errors = Vec::new();

    for file in &files {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

        match format_with_block_names_or_fallback(&content, &config, &block_names) {
            Ok(formatted) => {
                if content != formatted {
                    needs_formatting.push((file.clone(), content.clone(), formatted.clone()));

                    if show_diff {
                        print_diff(file, &content, &formatted);
                    }

                    if !check {
                        fs::write(file, &formatted)
                            .map_err(|e| format!("Failed to write {}: {}", file.display(), e))?;
                        println!("{} {}", "Formatted:".green(), file.display());
                    }
                }
            }
            Err(e) => {
                errors.push((file.clone(), e.to_string()));
            }
        }
    }

    // Print summary
    if check {
        if needs_formatting.is_empty() && errors.is_empty() {
            println!("{}", "All files are properly formatted.".green());
            Ok(())
        } else {
            if !needs_formatting.is_empty() {
                println!("{}", "The following files need formatting:".yellow());
                for (file, _, _) in &needs_formatting {
                    println!("  {}", file.display());
                }
            }
            for (file, err) in &errors {
                eprintln!("{} {}: {}", "Error:".red(), file.display(), err);
            }
            Err(AppError::Validation(
                "Some files are not properly formatted".to_string(),
            ))
        }
    } else if !errors.is_empty() {
        for (file, err) in &errors {
            eprintln!("{} {}: {}", "Error:".red(), file.display(), err);
        }
        Err(AppError::Validation(
            "Some files had formatting errors".to_string(),
        ))
    } else {
        let count = needs_formatting.len();
        if count > 0 {
            println!("{}", format!("Formatted {} file(s).", count).green().bold());
        } else {
            println!("{}", "All files are already properly formatted.".green());
        }
        Ok(())
    }
}

/// Format with block_names conversion. If format_with_block_names fails (e.g., parse error),
/// fall back to regular format which may produce a better error message.
fn format_with_block_names_or_fallback(
    content: &str,
    config: &FormatConfig,
    block_names: &HashMap<String, String>,
) -> Result<String, carina_core::formatter::FormatParseError> {
    formatter::format_with_block_names(content, config, block_names)
}

fn print_diff(file: &Path, original: &str, formatted: &str) {
    println!("\n{} {}:", "Diff for".cyan().bold(), file.display());

    let diff = TextDiff::from_lines(original, formatted);
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-".red(),
            ChangeTag::Insert => "+".green(),
            ChangeTag::Equal => " ".normal(),
        };
        print!("{}{}", sign, change);
    }
}
