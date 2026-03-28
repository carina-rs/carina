use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{find_crn_files_in_dir, get_base_dir, load_configuration};
use carina_core::lint::{
    find_duplicate_attrs, find_list_literal_attrs, find_non_snake_case_bindings,
    find_pipe_preferred_direct_calls, list_struct_attr_names,
};
use carina_core::module_resolver;
use carina_core::provider::{self as provider_mod};

use crate::error::AppError;
use crate::wiring::WiringContext;

/// A lint warning with file, line, and message info.
struct LintWarning {
    file: PathBuf,
    line: usize,
    message: String,
}

pub fn run_lint(path: &PathBuf) -> Result<(), AppError> {
    let mut parsed = load_configuration(path)?.parsed;

    let base_dir = get_base_dir(path);

    // Resolve modules
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    let ctx = WiringContext::new();
    let factories = ctx.factories();
    let schemas = ctx.schemas();

    // Collect source texts for each .crn file
    let source_texts: Vec<(PathBuf, String)> = if path.is_file() {
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
        return Err(AppError::Config(format!(
            "Path not found: {}",
            path.display()
        )));
    };

    // Collect all List<Struct> attribute names from schemas of parsed resources
    // and build a map of attr_name -> block_name for lint suggestions
    let mut all_list_struct_attrs: HashSet<String> = HashSet::new();
    let mut block_name_suggestions: HashMap<String, String> = HashMap::new();
    for resource in &parsed.resources {
        let schema_key = provider_mod::schema_key_for_resource(factories, resource);
        if let Some(schema) = schemas.get(&schema_key) {
            all_list_struct_attrs.extend(list_struct_attr_names(schema));
            for (attr_name, attr_schema) in &schema.attributes {
                if let Some(bn) = &attr_schema.block_name {
                    block_name_suggestions.insert(attr_name.clone(), bn.clone());
                }
            }
        }
    }

    // Scan each source file for list literal usage of List<Struct> attributes
    let mut warnings: Vec<LintWarning> = Vec::new();

    for (file_path, source) in &source_texts {
        // Check for list literal syntax on List<Struct> attributes
        let hits = find_list_literal_attrs(source, &all_list_struct_attrs);
        for (attr_name, line) in hits {
            let suggested_name = block_name_suggestions
                .get(&attr_name)
                .map(|s| s.as_str())
                .unwrap_or(&attr_name);
            warnings.push(LintWarning {
                file: file_path.clone(),
                line,
                message: format!(
                    "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                    attr_name, suggested_name, attr_name
                ),
            });
        }

        // Check for duplicate attribute keys within the same block
        let duplicates = find_duplicate_attrs(source);
        for dup in duplicates {
            warnings.push(LintWarning {
                file: file_path.clone(),
                line: dup.line,
                message: format!(
                    "Duplicate attribute '{}' (first defined on line {}). The last value will be used.",
                    dup.name, dup.first_line
                ),
            });
        }

        // Check for direct calls to pipe-preferred functions
        let pipe_warnings = find_pipe_preferred_direct_calls(source);
        for pw in pipe_warnings {
            warnings.push(LintWarning {
                file: file_path.clone(),
                line: pw.line,
                message: format!(
                    "Consider using pipe form for '{}': data |> {}(...)",
                    pw.name, pw.name
                ),
            });
        }

        // Check for non-snake_case binding names
        let naming_warnings = find_non_snake_case_bindings(source);
        for nw in naming_warnings {
            warnings.push(LintWarning {
                file: file_path.clone(),
                line: nw.line,
                message: format!(
                    "Binding '{}' is not snake_case. Use snake_case for binding names (e.g., 'my_resource').",
                    nw.name
                ),
            });
        }
    }

    if warnings.is_empty() {
        println!("{}", "No lint warnings found.".green().bold());
        Ok(())
    } else {
        for w in &warnings {
            eprintln!(
                "{} {}:{}  {}",
                "warning:".yellow().bold(),
                w.file.display(),
                w.line,
                w.message
            );
        }
        Err(AppError::Validation(format!(
            "Found {} lint warning(s).",
            warnings.len()
        )))
    }
}
