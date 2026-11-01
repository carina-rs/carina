use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(test)]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::config_loader::{
    find_crn_files_in_dir, find_crn_files_recursive, load_configuration,
};
use carina_core::formatter::{self, FormatConfig};
use carina_core::parser::{ProviderConfig as GenericProviderConfig, UseStatement};
use carina_core::paths::{CanonicalPath, ResolvedPath};
use carina_core::resource::ParserPlaceholder;
use carina_core::schema::collect_all_block_names;

use crate::error::AppError;
#[cfg(test)]
use crate::wiring::LoadedProviderFactory;
use crate::wiring::{FormattingProviderLoad, ProviderFactoryDiagnostic};

type ProviderConfig = GenericProviderConfig<ParserPlaceholder>;

/// Controls how formatting changes are applied and reported.
#[derive(Debug, Eq, PartialEq)]
pub enum FmtMode {
    /// Rewrites files in place.
    Write,
    /// Implements `--diff`: prints diffs, writes nothing, and exits 0 for formatting changes.
    Preview,
    /// Implements `--check`: writes nothing, fails on formatting changes, and may print diffs.
    Check { show_diff: bool },
}

impl FmtMode {
    /// Selects a formatting mode from the command-line flags, with `--check` taking precedence.
    pub fn from_flags(check: bool, diff: bool) -> Self {
        match (check, diff) {
            (false, false) => Self::Write,
            (false, true) => Self::Preview,
            (true, false) => Self::Check { show_diff: false },
            (true, true) => Self::Check { show_diff: true },
        }
    }

    fn print_diff_if_requested(&self, file: &Path, original: &str, formatted: &str) {
        match self {
            Self::Write | Self::Check { show_diff: false } => {}
            Self::Preview | Self::Check { show_diff: true } => {
                print_diff(file, original, formatted);
            }
        }
    }

    fn write_file_if_requested(
        &self,
        file: &ResolvedPath,
        label: &Path,
        formatted: &str,
    ) -> Result<(), AppError> {
        match self {
            Self::Write => {
                file.atomic_replace(formatted.as_bytes())
                    .map_err(|e| format!("Failed to write {}: {}", label.display(), e))?;
                println!("{} {}", "Formatted:".green(), label.display());
                Ok(())
            }
            Self::Preview | Self::Check { .. } => Ok(()),
        }
    }

    fn warn_if_providers_unavailable(&self, diagnostics: &BTreeSet<String>) {
        match self {
            Self::Write => {}
            Self::Preview | Self::Check { .. } => {
                if !diagnostics.is_empty() {
                    eprintln!(
                        "{}",
                        "Warning: provider(s) unavailable; block-syntax conversion skipped"
                            .yellow()
                    );
                    for diagnostic in diagnostics {
                        eprintln!("  {diagnostic}");
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct DirectoryBlockNames {
    block_names: HashMap<String, String>,
    provider_load_diagnostics: HashMap<String, ProviderFactoryDiagnostic>,
}

#[derive(Default)]
struct FormattingDirectoryConfig {
    providers: Vec<ProviderConfig>,
    uses: Vec<UseStatement>,
}

fn load_formatting_directory_config(directory: &ResolvedPath) -> FormattingDirectoryConfig {
    let Some(loaded) = load_configuration(directory).ok() else {
        return FormattingDirectoryConfig::default();
    };
    let parsed = loaded.parsed.into_parsed();
    FormattingDirectoryConfig {
        providers: parsed.providers,
        uses: parsed.uses,
    }
}

fn block_names_from_config<F>(
    directory: &ResolvedPath,
    config: &FormattingDirectoryConfig,
    build_factories: &mut F,
) -> Result<DirectoryBlockNames, AppError>
where
    F: FnMut(&[ProviderConfig], &ResolvedPath) -> Result<FormattingProviderLoad, AppError>,
{
    if config.providers.is_empty() {
        return Ok(DirectoryBlockNames::default());
    }

    // Coverage is split at this seam: unit tests prove builder output feeds
    // block-name collection, and wiring tests pin the production builder. Its
    // one-line application here is the remaining integration seam because the
    // workspace mock WASM exports no schemas for a real conversion end-to-end.
    let (schemas, load_errors) = build_factories(&config.providers, directory)?.into_parts();
    Ok(DirectoryBlockNames {
        block_names: collect_all_block_names(&schemas),
        provider_load_diagnostics: load_errors,
    })
}

#[cfg(test)]
fn block_names_for_dir<F>(
    directory: &ResolvedPath,
    mut build_factories: F,
) -> Result<DirectoryBlockNames, AppError>
where
    F: FnMut(&[ProviderConfig], &ResolvedPath) -> Result<FormattingProviderLoad, AppError>,
{
    let config = load_formatting_directory_config(directory);
    block_names_from_config(directory, &config, &mut build_factories)
}

type DirectoryConfigs = HashMap<CanonicalPath, (ResolvedPath, FormattingDirectoryConfig)>;
type DirectoryImportMap = HashMap<CanonicalPath, BTreeMap<CanonicalPath, ResolvedPath>>;

fn invocation_directory_configs(
    root: &ResolvedPath,
    files: &[ResolvedPath],
    recursive: bool,
) -> Result<DirectoryConfigs, AppError> {
    let directories = if recursive {
        files
            .iter()
            .map(|file| file.parent().unwrap_or_else(|| root.clone()))
            .collect()
    } else {
        vec![root.clone()]
    };
    let mut configs = HashMap::new();
    for directory in directories {
        configs.entry(directory.identity()).or_insert_with(|| {
            let config = load_formatting_directory_config(&directory);
            (directory, config)
        });
    }
    Ok(configs)
}

fn imported_module_directory(
    caller_directory: &ResolvedPath,
    import: &UseStatement,
) -> std::io::Result<CanonicalPath> {
    let module_path = caller_directory.join(&import.path)?;
    let directory = if module_path.is_dir() {
        module_path
    } else if module_path
        .display()
        .extension()
        .is_some_and(|ext| ext == "crn")
    {
        module_path.parent().unwrap_or(module_path)
    } else {
        let with_extension = module_path.with_extension("crn")?;
        if with_extension.exists() {
            with_extension.parent().unwrap_or(module_path)
        } else {
            module_path
        }
    };
    Ok(directory.identity())
}

fn invocation_import_map(directories: &DirectoryConfigs) -> DirectoryImportMap {
    let mut import_map = DirectoryImportMap::new();
    for (identity, (caller, config)) in directories {
        for import in &config.uses {
            if let Ok(module) = imported_module_directory(caller, import) {
                import_map
                    .entry(module)
                    .or_default()
                    .insert(identity.clone(), caller.clone());
            }
        }
    }
    import_map
}

fn nearest_provider_directory(
    directory: &ResolvedPath,
    directories: &DirectoryConfigs,
) -> Option<ResolvedPath> {
    for identity in directory.identity().ancestors() {
        if let Some((path, config)) = directories.get(&identity)
            && !config.providers.is_empty()
        {
            return Some(path.clone());
        }
    }
    None
}

fn resolve_directory_block_names<F>(
    directory: &ResolvedPath,
    directories: &DirectoryConfigs,
    import_map: &DirectoryImportMap,
    cache: &mut HashMap<CanonicalPath, Arc<DirectoryBlockNames>>,
    build_factories: &mut F,
) -> Result<Arc<DirectoryBlockNames>, AppError>
where
    F: FnMut(&[ProviderConfig], &ResolvedPath) -> Result<FormattingProviderLoad, AppError>,
{
    let identity = directory.identity();
    if let Some(block_names) = cache.get(&identity) {
        return Ok(Arc::clone(block_names));
    }
    if let Some((path, config)) = directories
        .get(&identity)
        .filter(|(_, config)| !config.providers.is_empty())
    {
        let block_names = Arc::new(block_names_from_config(path, config, build_factories)?);
        cache.insert(identity, Arc::clone(&block_names));
        return Ok(block_names);
    }
    let caller = import_map.get(&identity).and_then(|callers| {
        callers
            .values()
            .find_map(|caller| nearest_provider_directory(caller, directories))
    });
    let block_names = match caller {
        Some(caller) => {
            resolve_directory_block_names(&caller, directories, import_map, cache, build_factories)?
        }
        None => Arc::new(DirectoryBlockNames::default()),
    };
    cache.insert(identity, Arc::clone(&block_names));
    Ok(block_names)
}

/// Format `.crn` files without making provider availability a hard dependency.
///
/// Provider load failures silently degrade affected block-syntax conversion to
/// plain formatting. In non-writing modes (`--check` and/or `--diff`), the same
/// degradation also emits a warning so results remain diagnosable. Recursive
/// formatting lets modules inherit schemas from caller directories visible in
/// the same walk. Formatting a module directory alone remains plain because
/// that invocation has no caller context, matching an LSP session opened
/// without the caller workspace.
pub fn run_fmt(
    user_dirs: &carina_core::UserDirs,
    path: &Path,
    mode: FmtMode,
    recursive: bool,
) -> Result<(), AppError> {
    // Schema-aware conversion costs one installed-WASM provider load per
    // provider-declaring directory visible to the invocation. The startup cost
    // is intentional: editor and CLI formatting correctness must win over the
    // old schema-free fast path.
    run_fmt_with_factory_builder(path, mode, recursive, |providers, directory| {
        FormattingProviderLoad::load(user_dirs, providers, directory)
    })
}

fn run_fmt_with_factory_builder<F>(
    path: &Path,
    mode: FmtMode,
    recursive: bool,
    mut build_factories: F,
) -> Result<(), AppError>
where
    F: FnMut(&[ProviderConfig], &ResolvedPath) -> Result<FormattingProviderLoad, AppError>,
{
    let root = ResolvedPath::new(path).map_err(|error| AppError::Config(error.to_string()))?;
    if root.is_file() {
        return Err(AppError::Config(format!(
            "expected directory, got file: {}",
            path.display()
        )));
    }

    let config = FormatConfig::default();
    let files = if recursive {
        find_crn_files_recursive(&root)?
    } else {
        find_crn_files_in_dir(&root)?
    };

    if files.is_empty() {
        println!("{}", "No .crn files found.".yellow());
        return Ok(());
    }

    let mut needs_formatting = Vec::new();
    let mut errors = Vec::new();
    let directory_configs = invocation_directory_configs(&root, &files, recursive)?;
    let import_map = invocation_import_map(&directory_configs);
    let mut block_names_by_dir: HashMap<CanonicalPath, Arc<DirectoryBlockNames>> = HashMap::new();
    let mut provider_load_diagnostics = BTreeSet::new();

    for file in &files {
        // Listing spelling belongs to the CLI argument; it is never used for IO.
        let label = path.join(
            file.display()
                .relative_to(root.display())
                .unwrap_or_else(|| file.display().to_string()),
        );
        let content = file
            .read_to_string()
            .map_err(|e| format!("Failed to read {}: {}", label.display(), e))?;
        let block_names = resolve_directory_block_names(
            &file.parent().unwrap_or_else(|| root.clone()),
            &directory_configs,
            &import_map,
            &mut block_names_by_dir,
            &mut build_factories,
        )?;
        provider_load_diagnostics.extend(
            block_names
                .provider_load_diagnostics
                .iter()
                .map(|(name, diagnostic)| format!("{name}: {diagnostic}")),
        );

        match formatter::format_with_block_names(&content, &config, &block_names.block_names) {
            Ok(formatted) => {
                if content != formatted {
                    needs_formatting.push((label.clone(), content.clone(), formatted.clone()));

                    mode.print_diff_if_requested(&label, &content, &formatted);
                    mode.write_file_if_requested(file, &label, &formatted)?;
                }
            }
            Err(e) => {
                errors.push((label.clone(), e.to_string()));
            }
        }
    }

    mode.warn_if_providers_unavailable(&provider_load_diagnostics);

    // Print summary
    match mode {
        FmtMode::Check { .. } => {
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
        }
        FmtMode::Preview => {
            report_formatting_errors(&errors)?;
            let count = needs_formatting.len();
            if count > 0 {
                println!(
                    "{}",
                    format!("{} file(s) would be reformatted.", count)
                        .yellow()
                        .bold()
                );
            } else {
                println!("{}", "All files are already properly formatted.".green());
            }
            Ok(())
        }
        FmtMode::Write => {
            report_formatting_errors(&errors)?;
            let count = needs_formatting.len();
            if count > 0 {
                println!("{}", format!("Formatted {} file(s).", count).green().bold());
            } else {
                println!("{}", "All files are already properly formatted.".green());
            }
            Ok(())
        }
    }
}

fn report_formatting_errors(errors: &[(PathBuf, String)]) -> Result<(), AppError> {
    if errors.is_empty() {
        return Ok(());
    }

    for (file, err) in errors {
        eprintln!("{} {}: {}", "Error:".red(), file.display(), err);
    }
    Err(AppError::Validation(
        "Some files had formatting errors".to_string(),
    ))
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
    use carina_core::provider::{BoxFuture, ProviderFactory, ProviderResult};
    use carina_core::resource::Value;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
    use indexmap::IndexMap;

    #[cfg(unix)]
    #[test]
    fn provider_diagnostics_preserve_symlink_directory() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        fs::write(real.join("main.crn"), "provider mock {}\n").unwrap();
        let error = run_fmt_with_factory_builder(&link, FmtMode::Preview, false, |_, directory| {
            Err(AppError::Config(directory.display().to_string()))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains(&link.display().to_string()), "{error}");
        assert!(!error.contains(&real.display().to_string()), "{error}");
    }

    #[test]
    fn fmt_mode_from_flags_covers_all_combinations() {
        assert_eq!(FmtMode::from_flags(false, false), FmtMode::Write);
        assert_eq!(FmtMode::from_flags(false, true), FmtMode::Preview);
        assert_eq!(
            FmtMode::from_flags(true, false),
            FmtMode::Check { show_diff: false }
        );
        assert_eq!(
            FmtMode::from_flags(true, true),
            FmtMode::Check { show_diff: true }
        );
    }

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

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            String::new()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
            _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
            Box::pin(async { unreachable!("formatting does not instantiate providers") })
        }

        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
                let entries = AttributeType::list(AttributeType::struct_(
                    "Entry",
                    vec![StructField::optional("value", AttributeType::string()).build()],
                ));
                vec![
                    ResourceSchema::new("example.resource").attribute(
                        AttributeSchema::optional("entries", entries)
                            .with_block_name(self.block_name)
                            .build(),
                    ),
                ]
            };
            schema_drafts
                .into_iter()
                .map(|schema| {
                    schema
                        .validate()
                        .expect("factory test schema must be valid")
                })
                .collect()
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

        let directory = block_names_for_dir(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            |providers, base_dir| {
                loaded_provider_names
                    .extend(providers.iter().map(|provider| provider.name.clone()));
                assert_eq!(
                    base_dir.identity(),
                    ResolvedPath::new(tmp.path()).unwrap().identity()
                );
                let factories: Vec<Box<dyn ProviderFactory>> =
                    vec![Box::new(FactoryWithBlockSchema {
                        block_name: "entry_from_factory",
                    })];
                Ok(FormattingProviderLoad::for_test(
                    LoadedProviderFactory::for_test(factories),
                    providers,
                    HashMap::new(),
                ))
            },
        )
        .unwrap();

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

        let error = run_fmt_with_factory_builder(&file, FmtMode::Write, false, |providers, _| {
            builder_calls += 1;
            Ok(FormattingProviderLoad::for_test(
                Vec::new(),
                providers,
                HashMap::new(),
            ))
        })
        .expect_err("a file path must be rejected");

        assert!(error.to_string().contains("expected directory, got file"));
        assert_eq!(builder_calls, 0);
    }

    #[test]
    fn provider_factory_builder_error_fails_formatting_command() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.crn"), "provider example {}\n").unwrap();

        let error = run_fmt_with_factory_builder(tmp.path(), FmtMode::Write, false, |_, _| {
            Err(AppError::from(
                carina_provider_resolver::GrantParseError::MissingSeparator {
                    value: "not-a-grant".to_string(),
                },
            ))
        })
        .expect_err("invalid grant input must fail once provider loading begins");

        assert!(matches!(error, AppError::TrustedLocalGrant(_)));
    }

    #[test]
    fn directory_unavailable_status_comes_from_load_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.crn"), "provider example {}\n").unwrap();

        let skipped = block_names_for_dir(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            |providers, _| {
                Ok(FormattingProviderLoad::for_test(
                    Vec::<LoadedProviderFactory>::new(),
                    providers,
                    HashMap::new(),
                ))
            },
        )
        .unwrap();
        let failed = block_names_for_dir(
            &carina_core::paths::ResolvedPath::new(tmp.path()).unwrap(),
            |providers, _| {
                Ok(FormattingProviderLoad::for_test(
                    Vec::<LoadedProviderFactory>::new(),
                    providers,
                    HashMap::from([(
                        "example".to_string(),
                        ProviderFactoryDiagnostic::Message("load failed".into()),
                    )]),
                ))
            },
        )
        .unwrap();

        assert!(skipped.provider_load_diagnostics.is_empty());
        assert!(!failed.provider_load_diagnostics.is_empty());
    }

    fn write_provider_caller(directory: &ResolvedPath, provider: &str, module_path: &str) {
        directory.create_dir_all().unwrap();
        std::fs::write(
            std::path::PathBuf::from(directory.display().join("main.crn").to_string()),
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

    fn write_convertible_module(directory: &ResolvedPath) -> PathBuf {
        directory.create_dir_all().unwrap();
        std::fs::write(
            std::path::PathBuf::from(directory.display().join("arguments.crn").to_string()),
            "arguments {\n  name: String\n}\n",
        )
        .unwrap();
        let resource =
            std::path::PathBuf::from(directory.display().join("resource.crn").to_string());
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
        let resource =
            write_convertible_module(&carina_core::paths::ResolvedPath::new(&module).unwrap());
        write_provider_caller(
            &carina_core::paths::ResolvedPath::new(tmp.path().join("z-caller")).unwrap(),
            "z",
            "../modules/m",
        );
        write_provider_caller(
            &carina_core::paths::ResolvedPath::new(tmp.path().join("a-caller")).unwrap(),
            "a",
            "../modules/m",
        );
        let mut built_for = Vec::new();

        run_fmt_with_factory_builder(tmp.path(), FmtMode::Write, true, |providers, base_dir| {
            let directory_name = base_dir
                .display()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap();
            built_for.push(directory_name.to_string());
            let block_name = match directory_name {
                "a-caller" => "entry_from_a",
                "z-caller" => "entry_from_z",
                other => panic!("unexpected provider directory: {other}"),
            };
            let factories: Vec<Box<dyn ProviderFactory>> =
                vec![Box::new(FactoryWithBlockSchema { block_name })];
            Ok(FormattingProviderLoad::for_test(
                LoadedProviderFactory::for_test(factories),
                providers,
                HashMap::new(),
            ))
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
        let resource =
            write_convertible_module(&carina_core::paths::ResolvedPath::new(&module).unwrap());
        let mut builder_calls = 0;

        run_fmt_with_factory_builder(&module, FmtMode::Write, false, |providers, _| {
            builder_calls += 1;
            Ok(FormattingProviderLoad::for_test(
                Vec::new(),
                providers,
                HashMap::new(),
            ))
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

        run_fmt_with_factory_builder(tmp.path(), FmtMode::Write, true, |providers, base_dir| {
            let directory_name = base_dir
                .display()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap();
            built_for.push(directory_name.to_string());
            let block_name = match directory_name {
                "a" => "entries_a",
                "b" => "entries_b",
                other => panic!("unexpected provider directory: {other}"),
            };
            let factories: Vec<Box<dyn ProviderFactory>> =
                vec![Box::new(FactoryWithBlockSchema { block_name })];
            Ok(FormattingProviderLoad::for_test(
                LoadedProviderFactory::for_test(factories),
                providers,
                HashMap::new(),
            ))
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
