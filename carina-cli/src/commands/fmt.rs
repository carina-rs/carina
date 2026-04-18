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
    let ctx = WiringContext::new(vec![]);
    let block_names = collect_all_block_names(ctx.schemas());

    let files = if path.is_file() {
        if path.extension().is_some_and(|ext| ext == "crn") {
            vec![path.clone()]
        } else {
            return Err(AppError::Config(format!(
                "expected a .crn file or a directory, got: {}",
                path.display()
            )));
        }
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

        match formatter::format_with_block_names(&content, &config, &block_names) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const UNFORMATTED: &str = "provider awscc {\nregion = awscc.Region.ap_northeast_1\n}\n";

    #[test]
    fn run_fmt_accepts_single_file_path() {
        // Scope: #1997 — `carina fmt <file.crn>` should not be rejected just
        // because the path is a file. Users who want to format one file must
        // not be forced to operate on the parent directory.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("example.crn");
        std::fs::write(&file, UNFORMATTED).unwrap();

        run_fmt(&file, false, false, false).expect("formatting a single .crn file must succeed");

        let after = std::fs::read_to_string(&file).unwrap();
        assert_ne!(
            after, UNFORMATTED,
            "the file should have been rewritten to the formatted form"
        );
    }

    #[test]
    fn run_fmt_single_file_check_mode_reports_unformatted() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("needs_format.crn");
        std::fs::write(&file, UNFORMATTED).unwrap();

        let result = run_fmt(&file, true, false, false);
        assert!(
            result.is_err(),
            "check mode must report an unformatted file with an error"
        );

        // The file must not be rewritten in check mode.
        let after = std::fs::read_to_string(&file).unwrap();
        assert_eq!(after, UNFORMATTED, "check mode must not modify the file");
    }

    #[test]
    fn run_fmt_rejects_non_crn_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "hello\n").unwrap();

        let result = run_fmt(&file, false, false, false);
        assert!(
            result.is_err(),
            "non-.crn files must still be rejected even though files are now accepted"
        );
    }
}
