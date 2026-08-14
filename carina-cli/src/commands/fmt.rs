use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::config_loader::{
    find_crn_files_in_dir, find_crn_files_recursive, load_configuration,
};
use carina_core::formatter::{self, FormatConfig};
use carina_core::parser::{ProviderConfig, UseStatement};
use carina_core::schema::collect_all_block_names;

use crate::error::AppError;
use crate::wiring::{
    FormattingFactoryLoad, WiringContext, build_factories_from_providers_for_formatting,
};

#[derive(Default)]
struct DirectoryBlockNames {
    block_names: HashMap<String, String>,
    provider_load_diagnostics: HashMap<String, crate::wiring::ProviderFactoryDiagnostic>,
}

#[derive(Default)]
struct FormattingDirectoryConfig {
    providers: Vec<ProviderConfig>,
    uses: Vec<UseStatement>,
}

fn load_formatting_directory_config(directory: &Path) -> FormattingDirectoryConfig {
    let Some(loaded) = load_configuration(directory).ok() else {
        return FormattingDirectoryConfig::default();
    };
    FormattingDirectoryConfig {
        providers: loaded.parsed.providers,
        uses: loaded.parsed.uses,
    }
}

fn block_names_from_config<F>(
    directory: &Path,
    config: &FormattingDirectoryConfig,
    build_factories: &mut F,
) -> DirectoryBlockNames
where
    F: FnMut(&[ProviderConfig], &Path) -> FormattingFactoryLoad,
{
    if config.providers.is_empty() {
        return DirectoryBlockNames::default();
    }

    // Coverage is split at this seam: unit tests prove builder output feeds
    // block-name collection, and wiring tests pin the production builder. Its
    // one-line application here is the remaining integration seam because the
    // workspace mock WASM exports no schemas for a real conversion end-to-end.
    let (factories, load_errors) = build_factories(&config.providers, directory);
    let ctx = WiringContext::new(factories);
    DirectoryBlockNames {
        block_names: collect_all_block_names(ctx.schemas()),
        provider_load_diagnostics: load_errors,
    }
}

#[cfg(test)]
fn block_names_for_dir<F>(directory: &Path, mut build_factories: F) -> DirectoryBlockNames
where
    F: FnMut(&[ProviderConfig], &Path) -> FormattingFactoryLoad,
{
    let config = load_formatting_directory_config(directory);
    block_names_from_config(directory, &config, &mut build_factories)
}

fn canonical_directory(directory: &Path) -> PathBuf {
    directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf())
}

fn invocation_directory_configs(
    path: &Path,
    files: &[PathBuf],
    recursive: bool,
) -> HashMap<PathBuf, FormattingDirectoryConfig> {
    let directories: BTreeSet<PathBuf> = if recursive {
        files
            .iter()
            .map(|file| canonical_directory(file.parent().unwrap_or(path)))
            .collect()
    } else {
        [canonical_directory(path)].into_iter().collect()
    };

    directories
        .into_iter()
        .map(|directory| {
            let config = load_formatting_directory_config(&directory);
            (directory, config)
        })
        .collect()
}

fn imported_module_directory(caller_directory: &Path, import: &UseStatement) -> PathBuf {
    let module_path = caller_directory.join(&import.path);
    let module_directory = if module_path.is_dir() {
        module_path
    } else if module_path.extension().is_some_and(|ext| ext == "crn") {
        module_path.parent().unwrap_or(&module_path).to_path_buf()
    } else {
        let with_extension = module_path.with_extension("crn");
        if with_extension.exists() {
            with_extension
                .parent()
                .unwrap_or(&module_path)
                .to_path_buf()
        } else {
            module_path
        }
    };
    canonical_directory(&module_directory)
}

fn invocation_import_map(
    directories: &HashMap<PathBuf, FormattingDirectoryConfig>,
) -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut import_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (caller_directory, config) in directories {
        for import in &config.uses {
            import_map
                .entry(imported_module_directory(caller_directory, import))
                .or_default()
                .push(caller_directory.clone());
        }
    }
    for callers in import_map.values_mut() {
        callers.sort();
        callers.dedup();
    }
    import_map
}

fn nearest_provider_directory(
    directory: &Path,
    directories: &HashMap<PathBuf, FormattingDirectoryConfig>,
) -> Option<PathBuf> {
    let mut candidate = Some(directory);
    while let Some(current) = candidate {
        if directories
            .get(current)
            .is_some_and(|config| !config.providers.is_empty())
        {
            return Some(current.to_path_buf());
        }
        candidate = current.parent();
    }
    None
}

fn resolve_directory_block_names<F>(
    directory: &Path,
    directories: &HashMap<PathBuf, FormattingDirectoryConfig>,
    import_map: &HashMap<PathBuf, Vec<PathBuf>>,
    cache: &mut HashMap<PathBuf, Arc<DirectoryBlockNames>>,
    build_factories: &mut F,
) -> Arc<DirectoryBlockNames>
where
    F: FnMut(&[ProviderConfig], &Path) -> FormattingFactoryLoad,
{
    let directory = canonical_directory(directory);
    if let Some(block_names) = cache.get(&directory) {
        return Arc::clone(block_names);
    }

    if let Some(config) = directories
        .get(&directory)
        .filter(|config| !config.providers.is_empty())
    {
        let block_names = Arc::new(block_names_from_config(&directory, config, build_factories));
        cache.insert(directory, Arc::clone(&block_names));
        return block_names;
    }

    let caller_provider_directory = import_map.get(&directory).and_then(|callers| {
        callers
            .iter()
            .find_map(|caller| nearest_provider_directory(caller, directories))
    });
    let block_names = caller_provider_directory
        .map(|caller| {
            resolve_directory_block_names(&caller, directories, import_map, cache, build_factories)
        })
        .unwrap_or_else(|| Arc::new(DirectoryBlockNames::default()));
    cache.insert(directory, Arc::clone(&block_names));
    block_names
}

/// Format `.crn` files without making provider availability a hard dependency.
///
/// Provider load failures silently degrade affected block-syntax conversion to
/// plain formatting. In `--check` mode, the same degradation also emits a
/// warning so CI results remain diagnosable. Recursive formatting lets modules
/// inherit schemas from caller directories visible in the same walk. Formatting
/// a module directory alone remains plain because that invocation has no caller
/// context, matching an LSP session opened without the caller workspace.
pub fn run_fmt(path: &Path, check: bool, show_diff: bool, recursive: bool) -> Result<(), AppError> {
    // Schema-aware conversion costs one installed-WASM provider load per
    // provider-declaring directory visible to the invocation. The startup cost
    // is intentional: editor and CLI formatting correctness must win over the
    // old schema-free fast path.
    run_fmt_with_factory_builder(
        path,
        check,
        show_diff,
        recursive,
        build_factories_from_providers_for_formatting,
    )
}

fn run_fmt_with_factory_builder<F>(
    path: &Path,
    check: bool,
    show_diff: bool,
    recursive: bool,
    mut build_factories: F,
) -> Result<(), AppError>
where
    F: FnMut(&[ProviderConfig], &Path) -> FormattingFactoryLoad,
{
    if path.is_file() {
        return Err(AppError::Config(format!(
            "expected directory, got file: {}",
            path.display()
        )));
    }

    let config = FormatConfig::default();
    let files = if recursive {
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
    let directory_configs = invocation_directory_configs(path, &files, recursive);
    let import_map = invocation_import_map(&directory_configs);
    let mut block_names_by_dir: HashMap<PathBuf, Arc<DirectoryBlockNames>> = HashMap::new();
    let mut provider_load_diagnostics = BTreeSet::new();

    for file in &files {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
        let block_names = resolve_directory_block_names(
            file.parent().unwrap_or(path),
            &directory_configs,
            &import_map,
            &mut block_names_by_dir,
            &mut build_factories,
        );
        provider_load_diagnostics.extend(
            block_names
                .provider_load_diagnostics
                .iter()
                .map(|(name, diagnostic)| format!("{name}: {diagnostic}")),
        );

        match formatter::format_with_block_names(&content, &config, &block_names.block_names) {
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

    if check && !provider_load_diagnostics.is_empty() {
        eprintln!(
            "{}",
            "Warning: provider(s) unavailable; block-syntax conversion skipped".yellow()
        );
        for diagnostic in provider_load_diagnostics {
            eprintln!("  {diagnostic}");
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
    use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
    use carina_core::resource::Value;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    use indexmap::IndexMap;

    struct FactoryWithBlockSchema {
        block_name: &'static str,
    }

    impl ProviderFactory for FactoryWithBlockSchema {
        fn name(&self) -> &str {
            "example"
        }

        fn display_name(&self) -> &str {
            "Example"
        }

        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::new()
        }

        fn validate_config(
            &self,
            _attributes: &IndexMap<String, Value>,
        ) -> std::result::Result<(), String> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            String::new()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _attributes: &IndexMap<String, Value>,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async { unreachable!("formatting does not instantiate providers") })
        }

        fn schemas(&self) -> Vec<ResourceSchema> {
            let entries = AttributeType::list(AttributeType::struct_(
                "Entry",
                vec![StructField::new("value", AttributeType::string())],
            ));
            vec![ResourceSchema::new("example.resource").attribute(
                AttributeSchema::new("entries", entries).with_block_name(self.block_name),
            )]
        }
    }

    #[test]
    fn directory_block_names_use_factories_built_from_declared_providers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("main.crn"),
            r#"provider example {
  source = "file:///missing-provider.wasm"
}
"#,
        )
        .unwrap();
        let mut loaded_provider_names = Vec::new();

        let directory = block_names_for_dir(tmp.path(), |providers, base_dir| {
            loaded_provider_names.extend(providers.iter().map(|provider| provider.name.clone()));
            assert_eq!(base_dir, tmp.path());
            let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(FactoryWithBlockSchema {
                block_name: "entry_from_factory",
            })];
            (factories, HashMap::new())
        });

        assert_eq!(loaded_provider_names, ["example"]);
        assert_eq!(
            directory.block_names.get("entries").map(String::as_str),
            Some("entry_from_factory")
        );
        assert!(directory.provider_load_diagnostics.is_empty());
    }

    #[test]
    fn file_path_is_rejected_before_factory_builder_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("main.crn");
        std::fs::write(
            &file,
            r#"provider example {
  source = "file:///missing-provider.wasm"
}
"#,
        )
        .unwrap();
        let mut builder_calls = 0;

        let error = run_fmt_with_factory_builder(&file, false, false, false, |_, _| {
            builder_calls += 1;
            (Vec::new(), HashMap::new())
        })
        .expect_err("a file path must be rejected");

        assert!(error.to_string().contains("expected directory, got file"));
        assert_eq!(builder_calls, 0);
    }

    #[test]
    fn directory_unavailable_status_comes_from_load_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.crn"), "provider example {}\n").unwrap();

        let skipped = block_names_for_dir(tmp.path(), |_, _| {
            (Vec::<Box<dyn ProviderFactory>>::new(), HashMap::new())
        });
        let failed = block_names_for_dir(tmp.path(), |_, _| {
            (
                Vec::<Box<dyn ProviderFactory>>::new(),
                HashMap::from([("example".to_string(), "load failed".to_string().into())]),
            )
        });

        assert!(skipped.provider_load_diagnostics.is_empty());
        assert!(!failed.provider_load_diagnostics.is_empty());
    }

    fn write_provider_caller(directory: &Path, provider: &str, module_path: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join("main.crn"),
            format!(
                r#"provider {provider} {{}}

let imported = use {{
  source = '{module_path}'
}}
"#
            ),
        )
        .unwrap();
    }

    fn write_convertible_module(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join("arguments.crn"),
            "arguments {\n  name: String\n}\n",
        )
        .unwrap();
        let resource = directory.join("resource.crn");
        std::fs::write(
            &resource,
            r#"let target = example.test.resource {
  entries = [{
    value = "one"
  }]
}
"#,
        )
        .unwrap();
        resource
    }

    #[test]
    fn recursive_module_uses_lexicographically_first_callers_block_names() {
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("modules").join("m");
        let resource = write_convertible_module(&module);
        write_provider_caller(&tmp.path().join("z-caller"), "z", "../modules/m");
        write_provider_caller(&tmp.path().join("a-caller"), "a", "../modules/m");
        let mut built_for = Vec::new();

        run_fmt_with_factory_builder(tmp.path(), false, false, true, |_, base_dir| {
            let directory_name = base_dir.file_name().and_then(|name| name.to_str()).unwrap();
            built_for.push(directory_name.to_string());
            let block_name = match directory_name {
                "a-caller" => "entry_from_a",
                "z-caller" => "entry_from_z",
                other => panic!("unexpected provider directory: {other}"),
            };
            let factories: Vec<Box<dyn ProviderFactory>> =
                vec![Box::new(FactoryWithBlockSchema { block_name })];
            (factories, HashMap::new())
        })
        .unwrap();

        let formatted = std::fs::read_to_string(resource).unwrap();
        assert!(formatted.contains("  entry_from_a {\n"), "{formatted}");
        assert!(!formatted.contains("entry_from_z"), "{formatted}");
        built_for.sort();
        assert_eq!(built_for, ["a-caller", "z-caller"]);
    }

    #[test]
    fn non_recursive_module_without_caller_in_scope_stays_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let module = tmp.path().join("module");
        let resource = write_convertible_module(&module);
        let mut builder_calls = 0;

        run_fmt_with_factory_builder(&module, false, false, false, |_, _| {
            builder_calls += 1;
            (Vec::new(), HashMap::new())
        })
        .unwrap();

        let formatted = std::fs::read_to_string(resource).unwrap();
        assert!(formatted.contains("entries = ["), "{formatted}");
        assert!(!formatted.contains("entry_from_"), "{formatted}");
        assert_eq!(builder_calls, 0);
    }

    #[test]
    fn recursive_format_memoizes_block_names_by_exact_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut resources = Vec::new();
        for directory_name in ["a", "b"] {
            let directory = tmp.path().join(directory_name);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("providers.crn"),
                format!("provider {directory_name} {{}}\n"),
            )
            .unwrap();
            let resource = directory.join("resource.crn");
            std::fs::write(
                &resource,
                r#"let target = example.test.resource {
  entries = [{ value = "one" }]
}
"#,
            )
            .unwrap();
            resources.push((directory_name, resource));
        }
        let mut built_for = Vec::new();

        run_fmt_with_factory_builder(tmp.path(), false, false, true, |_, base_dir| {
            let directory_name = base_dir.file_name().and_then(|name| name.to_str()).unwrap();
            built_for.push(directory_name.to_string());
            let block_name = match directory_name {
                "a" => "entries_a",
                "b" => "entries_b",
                other => panic!("unexpected provider directory: {other}"),
            };
            let factories: Vec<Box<dyn ProviderFactory>> =
                vec![Box::new(FactoryWithBlockSchema { block_name })];
            (factories, HashMap::new())
        })
        .unwrap();

        for (directory_name, resource) in resources {
            let formatted = std::fs::read_to_string(resource).unwrap();
            assert!(
                formatted.contains(&format!("  entries_{directory_name} {{\n")),
                "{formatted}"
            );
        }
        built_for.sort();
        assert_eq!(built_for, ["a", "b"]);
    }
}
