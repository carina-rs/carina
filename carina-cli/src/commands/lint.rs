use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::{
    TagKeyEntry, collect_tag_keys, find_duplicate_attrs, find_list_literal_attrs,
    find_mixed_tag_key_styles, find_non_snake_case_bindings, find_pipe_preferred_direct_calls,
    list_struct_attr_names,
};
use carina_core::module_resolver;
use carina_core::parser::ProviderContext;
use carina_core::provider::{self as provider_mod};

use crate::error::AppError;
use crate::wiring::{WiringContext, build_factories_from_providers};

/// A lint warning with file, line, and message info.
struct LintWarning {
    file: PathBuf,
    line: usize,
    message: String,
}

pub fn run_lint(path: &PathBuf, provider_context: &ProviderContext) -> Result<(), AppError> {
    let mut parsed = load_configuration_with_config(path, provider_context)?.parsed;

    let base_dir = get_base_dir(path);

    // Resolve modules
    module_resolver::resolve_modules_with_config(&mut parsed, base_dir, provider_context)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    let provider_factories = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(provider_factories);
    let factories = ctx.factories();
    let schemas = ctx.schemas();

    // Collect source texts for each .crn file
    let source_texts: Vec<(PathBuf, String)> = {
        let files = find_crn_files_in_dir(path)?;
        let mut texts = Vec::new();
        for file in files {
            let content = fs::read_to_string(&file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            texts.push((file, content));
        }
        texts
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
    // Collect tag keys across all files for cross-file consistency check
    let mut all_tag_keys: Vec<TagKeyEntry> = Vec::new();
    let mut tag_key_files: Vec<PathBuf> = Vec::new();

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

        // Collect tag keys for cross-file consistency check
        let file_tag_keys = collect_tag_keys(source);
        for entry in &file_tag_keys {
            all_tag_keys.push(TagKeyEntry {
                key: entry.key.clone(),
                style: entry.style,
                line: entry.line,
            });
            tag_key_files.push(file_path.clone());
        }
    }

    // Also collect tag keys from module directories
    for call in &parsed.module_calls {
        let module_dir = base_dir.join(&call.module_name);
        if module_dir.is_dir()
            && let Ok(module_files) = find_crn_files_in_dir(&module_dir)
        {
            for mf in module_files {
                if let Ok(content) = fs::read_to_string(&mf) {
                    let file_tag_keys = collect_tag_keys(&content);
                    for entry in &file_tag_keys {
                        all_tag_keys.push(TagKeyEntry {
                            key: entry.key.clone(),
                            style: entry.style,
                            line: entry.line,
                        });
                        tag_key_files.push(mf.clone());
                    }
                }
            }
        }
    }

    // Check for mixed tag key styles across all collected keys
    {
        let tag_warnings = find_mixed_tag_key_styles(&all_tag_keys);
        for tw in tag_warnings {
            let style_name = match tw.expected_style {
                carina_core::lint::TagKeyStyle::PascalCase => "PascalCase",
                carina_core::lint::TagKeyStyle::SnakeCase => "snake_case",
                carina_core::lint::TagKeyStyle::Other => "consistent",
            };
            // Find the file for this warning by matching line number
            let file = all_tag_keys
                .iter()
                .zip(tag_key_files.iter())
                .find(|(e, _)| e.key == tw.key && e.line == tw.line)
                .map(|(_, f)| f.clone())
                .unwrap_or_default();
            warnings.push(LintWarning {
                file,
                line: tw.line,
                message: format!(
                    "Tag key '{}' doesn't match the dominant style ({}). Use consistent casing for tag keys.",
                    tw.key, style_name
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
