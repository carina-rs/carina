use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::output_style::Colorize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::{
    TagKeyEntry, collect_all_tag_keys, find_duplicate_attrs, find_list_literal_attrs,
    find_mixed_tag_key_styles, find_non_snake_case_bindings, find_pipe_preferred_direct_calls,
    list_struct_attr_names, mixed_tag_key_style_message,
};
use carina_core::module_resolver;
use carina_core::parser::ProviderContext;

use crate::error::AppError;
use crate::module_walk::ModuleWalk;
use crate::wiring::{
    ProviderLoad, validate_no_backend_in_modules, validate_no_exports_in_modules,
    validate_no_provider_in_modules, validate_no_state_blocks_in_modules,
    validate_no_upstream_states_in_modules,
};

/// A lint warning with file, line, and message info.
struct LintWarning {
    file: PathBuf,
    line: usize,
    message: String,
}

fn mixed_tag_key_style_warnings(all_tag_keys: &[TagKeyEntry]) -> Vec<LintWarning> {
    find_mixed_tag_key_styles(all_tag_keys)
        .into_iter()
        .map(|warning| LintWarning {
            file: warning.file.clone().unwrap_or_default(),
            line: warning.line,
            message: mixed_tag_key_style_message(&warning),
        })
        .collect()
}

fn mixed_tag_key_style_warnings_for_population<E, D, B: carina_core::resource::BindingAddress>(
    root_inputs: &[(PathBuf, String)],
    parsed: &carina_core::parser::File<
        E,
        carina_core::resource::ParserPlaceholder,
        carina_core::resource::ParserPlaceholder,
        D,
        B,
    >,
    base_dir: &Path,
) -> Vec<LintWarning> {
    let tag_keys = collect_all_tag_keys(root_inputs, parsed, base_dir);
    mixed_tag_key_style_warnings(&tag_keys)
}

enum LintFile {
    Blocked(carina_core::config_loader::InferredRootDirectory),
    Resolved(carina_core::parser::ModuleResolvedInferredFile),
}

impl LintFile {
    fn resolve_type_exprs_best_effort(
        &mut self,
        context: &dyn carina_core::validation::deferred_types::ProviderTypeCheckContext,
    ) {
        match self {
            Self::Blocked(parsed) => {
                parsed.resolve_type_exprs_report(context);
            }
            Self::Resolved(parsed) => {
                carina_core::validation::resolve_file_type_exprs_best_effort(parsed, context);
            }
        }
    }
    fn export_handle_errors(&self) -> Option<Vec<carina_core::validation::ExportBoundaryError>> {
        match self {
            Self::Blocked(parsed) => {
                carina_core::validation::validate_export_handle_boundaries(&parsed.export_params)
                    .err()
            }
            Self::Resolved(parsed) => {
                carina_core::validation::validate_export_handle_boundaries(&parsed.export_params)
                    .err()
            }
        }
    }

    fn collect_list_struct_attrs(
        &self,
        schemas: &carina_core::schema::SchemaRegistry,
        all_list_struct_attrs: &mut HashSet<String>,
        block_name_suggestions: &mut HashMap<String, String>,
    ) {
        match self {
            Self::Blocked(parsed) => collect_list_struct_attrs_for_population(
                parsed.parsed(),
                schemas,
                all_list_struct_attrs,
                block_name_suggestions,
            ),
            Self::Resolved(parsed) => collect_list_struct_attrs_for_population(
                parsed,
                schemas,
                all_list_struct_attrs,
                block_name_suggestions,
            ),
        }
    }

    fn mixed_tag_key_style_warnings(
        &self,
        root_inputs: &[(PathBuf, String)],
        base_dir: &Path,
    ) -> Vec<LintWarning> {
        match self {
            Self::Blocked(parsed) => {
                mixed_tag_key_style_warnings_for_population(root_inputs, parsed.parsed(), base_dir)
            }
            Self::Resolved(parsed) => {
                mixed_tag_key_style_warnings_for_population(root_inputs, parsed, base_dir)
            }
        }
    }
}

fn collect_list_struct_attrs_for_population<E, D, B: carina_core::resource::BindingAddress>(
    parsed: &carina_core::parser::File<
        E,
        carina_core::resource::ParserPlaceholder,
        carina_core::resource::ParserPlaceholder,
        D,
        B,
    >,
    schemas: &carina_core::schema::SchemaRegistry,
    all_list_struct_attrs: &mut HashSet<String>,
    block_name_suggestions: &mut HashMap<String, String>,
) {
    for resource in parsed.iter_top_level_resources() {
        let schema = match resource {
            carina_core::parser::TopLevelResourceRef::Resource(resource) => {
                schemas.get_for(resource)
            }
            carina_core::parser::TopLevelResourceRef::DataSource(data_source) => {
                schemas.get_for_data_source(data_source)
            }
            carina_core::parser::TopLevelResourceRef::LiveComposition(_) => None,
        };
        if let Some(schema) = schema {
            let schema = schema.as_schema();
            all_list_struct_attrs.extend(list_struct_attr_names(schema));
            for (attribute_name, attribute_schema) in schema.attributes() {
                if let Some(block_name) = attribute_schema.block_name() {
                    block_name_suggestions.insert(attribute_name.clone(), block_name.to_string());
                }
            }
        }
    }
}

struct LintResult {
    checks: carina_core::validation::deferred_types::ProviderTypeCheckResult<
        Result<Vec<LintWarning>, AppError>,
    >,
}

struct LintTypeCheckSink<'a> {
    path: &'a Path,
    warnings: Vec<carina_core::parser::ParseWarning>,
    errors: Vec<carina_core::parser::ParseWarning>,
}
impl carina_core::validation::deferred_types::ProviderTypeCheckSink for LintTypeCheckSink<'_> {
    fn record(
        &mut self,
        diagnostic: carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic,
    ) {
        let (mut warning, is_error) = match diagnostic {
            carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(warning) => (warning, false),
            carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => (error, true),
        };
        if warning.file.is_none() {
            warning.file = Some(self.path.display().to_string());
        }
        if is_error {
            self.errors.push(warning);
        } else {
            self.warnings.push(warning);
        }
    }
}

impl LintResult {
    fn settle(
        checks: carina_core::validation::deferred_types::ProviderTypeCheckResult<
            Result<Vec<LintWarning>, AppError>,
        >,
    ) -> Self {
        Self { checks }
    }

    fn publish(
        self,
        path: &Path,
        color_mode: crate::output_style::ColorMode,
    ) -> Result<(), AppError> {
        let mut sink = LintTypeCheckSink {
            path,
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let result = self.checks.publish(&mut sink);
        let provider_warning_count = sink.warnings.len();
        for warning in &sink.warnings {
            crate::commands::validate::print_provider_type_warning(warning);
        }
        for error in &sink.errors {
            eprintln!(
                "{} {}:{}:{}: {}",
                "error:".red(color_mode).bold(color_mode),
                error.file.as_deref().unwrap_or("<config>"),
                error.line,
                error.span.as_ref().map_or(1, |span| span.start_column),
                error.message
            );
        }
        if !sink.errors.is_empty() {
            let mut errors = sink
                .errors
                .into_iter()
                .map(|error| error.message)
                .collect::<Vec<_>>();
            if let Err(error) = result {
                errors.insert(0, error.to_string());
            }
            return Err(AppError::Validation(errors.join("\n")));
        }
        let (warnings, result) = match result {
            Ok(findings) => (findings, Ok(())),
            Err(error) => (Vec::new(), Err(error)),
        };
        let warning_count = provider_warning_count + warnings.len();
        if warning_count == 0 {
            result?;
            println!(
                "{}",
                "No lint warnings found.".green(color_mode).bold(color_mode)
            );
            return Ok(());
        }
        for warning in &warnings {
            eprintln!(
                "{} {}:{}  {}",
                "warning:".yellow(color_mode).bold(color_mode),
                warning.file.display(),
                warning.line,
                warning.message
            );
        }
        result?;
        Err(AppError::Validation(format!(
            "Found {} lint warning(s).",
            warning_count
        )))
    }
}

pub fn run_lint(
    runtime: &crate::runtime::CommandRuntime,
    path: &Path,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    lint_result(runtime, path, provider_context)?.publish(path, runtime.color_mode())
}

fn lint_result(
    runtime: &crate::runtime::CommandRuntime,
    path: &Path,
    provider_context: &ProviderContext,
) -> Result<LintResult, AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            return Ok(LintResult::settle(
                crate::wiring::failed_configuration_report(path, error.into()),
            ));
        }
    };
    let duplicate_declarations = loaded.duplicate_declarations;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    let module_walk = ModuleWalk::load(&parsed, base_dir);
    let mut module_boundary_findings = validate_no_provider_in_modules(&module_walk);
    module_boundary_findings.extend(validate_no_state_blocks_in_modules(&module_walk));
    module_boundary_findings.extend(validate_no_backend_in_modules(&module_walk));
    module_boundary_findings.extend(validate_no_upstream_states_in_modules(&module_walk));
    module_boundary_findings.extend(validate_no_exports_in_modules(&module_walk));

    let (providers, provider_factory_error) =
        ProviderLoad::load_reporting(runtime, &parsed.providers, base_dir)?;
    let provider_factory_error = provider_factory_error.map(|error| error.to_string());
    let schemas = providers.schemas();
    let load_findings = providers.load_findings(&parsed.providers, false);
    let authority = providers.validation_catalog(&parsed.providers);
    let checks = carina_core::validation::deferred_types::TypeCheckSession::run(
        &authority,
        |catalog| {
            let mut type_findings = parsed
                .resolve_type_exprs_report(catalog)
                .into_diagnostics()
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            if type_findings.is_empty()
                && let Err(error) = carina_core::validation::validate_attribute_params_with_context(
                    &parsed.attribute_params,
                    catalog,
                )
            {
                type_findings.push(error);
            }
            if type_findings.is_empty()
                && let Err(error) = parsed.resolve_resource_refs_in_session(catalog)
            {
                type_findings.push(error.to_string());
            }
            for module in module_walk.iter() {
                type_findings.extend(
                    carina_core::validation::resolve_file_type_exprs(
                        &mut module.loaded().parsed.clone(),
                        catalog,
                    )
                    .into_iter()
                    .map(|error| error.to_string()),
                );
            }

            // Report invalid root-owned module blocks through lint's collected,
            // path-prefixed output. The resolver enforces the same invariants as
            // expansion backstops, so do not expand after this gate fails or lint
            // would abort before rendering the findings below. This deliberately
            // degrades schema-dependent checks: `iter_top_level_resources` then sees
            // only root-declared resources, so List<Struct> block-syntax suggestions
            // can disappear even from root files. The configuration is already
            // blocked; retaining these findings and the schema-independent text scans
            // is more useful than aborting lint entirely. Full schema-backed
            // suggestions return once the blocking finding is fixed.
            let mut parsed = if module_boundary_findings.is_empty() && type_findings.is_empty() {
                let resolution = module_resolver::resolve_module_template_with_config(
                    parsed, base_dir, catalog, schemas,
                );
                LintFile::Resolved(resolution.map_err(|e| {
                    format!(
                        "Module resolution error: {}",
                        e.display_relative_to(base_dir)
                    )
                })?)
            } else {
                LintFile::Blocked(parsed)
            };

            parsed.resolve_type_exprs_best_effort(catalog);
            let export_handle_errors = parsed.export_handle_errors();

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
            // carina#3181: walk every top-level resource — managed and data
            // source schemas can both carry List<Struct> attributes. compositions
            // have no schema, so `get_for` returns `None` for them.
            parsed.collect_list_struct_attrs(
                schemas,
                &mut all_list_struct_attrs,
                &mut block_name_suggestions,
            );

            // Scan each source file for list literal usage of List<Struct> attributes
            let mut warnings: Vec<LintWarning> = module_boundary_findings
                .into_iter()
                .map(|finding| LintWarning {
                    // The merged module parse does not retain an offending declaration
                    // span. Keep lint's file:line convention at the caller boundary;
                    // the finding itself retains the recursive module-path prefix.
                    file: path.to_path_buf(),
                    line: 1,
                    message: finding.to_string(),
                })
                .collect();
            if let Some(boundary_errors) = export_handle_errors {
                warnings.extend(boundary_errors.into_iter().map(|error| LintWarning {
                    file: error.source_file().to_path_buf(),
                    line: error.source_span().start_line(),
                    message: error.to_string(),
                }));
            }
            if let Some(message) = provider_factory_error {
                warnings.push(LintWarning {
                    file: path.to_path_buf(),
                    line: 1,
                    message,
                });
            }
            for message in type_findings
                .into_iter()
                .chain(load_findings.into_iter().map(|error| error.to_string()))
            {
                warnings.push(LintWarning {
                    file: path.to_path_buf(),
                    line: 1,
                    message,
                });
            }
            for duplicate in &duplicate_declarations {
                let file = duplicate
                    .first_file()
                    .map(|relative| path.join(relative))
                    .unwrap_or_else(|| path.to_path_buf());
                warnings.push(LintWarning {
                    file,
                    // Declaration AST nodes do not carry spans. Keep the finding in
                    // lint's normal file:line shape and anchor it at the file start;
                    // the message retains exact per-file provenance.
                    line: 1,
                    message: duplicate.to_string(),
                });
            }
            for module in module_walk.iter() {
                for duplicate in &module.loaded().duplicate_declarations {
                    let file = duplicate
                        .first_file()
                        .map(|relative| module.module_path().join(relative))
                        .unwrap_or_else(|| module.module_path().to_path_buf());
                    warnings.push(LintWarning {
                        file,
                        line: 1,
                        message: format!("{}: {duplicate}", module.diagnostic_path().display()),
                    });
                }
            }
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

            warnings.extend(parsed.mixed_tag_key_style_warnings(&source_texts, base_dir));

            Ok(warnings)
        },
    );
    Ok(LintResult::settle(checks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::lint::collect_tag_keys_for_file;
    use carina_core::parser::ParsedFile;

    #[test]
    fn lint_hard_provider_load_failure_still_reports_missing_sources() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "provider broken { source = 'file:///definitely-missing/broken.wasm' }\nprovider broken { source = 'file:///definitely-missing/broken.wasm' }\nprovider missing {}\n").unwrap();
        let runtime = crate::test_support::TestCommandRuntime::new();
        let result = lint_result(&runtime, fixture.path(), &ProviderContext::default()).unwrap();
        let mut sink = LintTypeCheckSink {
            path: fixture.path(),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        let findings = result.checks.publish(&mut sink).unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("declared more than once")),
            "{:?}",
            findings
                .iter()
                .map(|finding| &finding.message)
                .collect::<Vec<_>>()
        );
        assert!(
            findings.iter().any(|finding| finding.message
                == crate::commands::missing_provider_source_message("missing")),
            "{:?}",
            findings
                .iter()
                .map(|finding| &finding.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn lint_loaded_pending_check_preserves_the_producer_error() {
        use carina_core::validation::deferred_types::{ProviderTypeCheckSite, TypeCheckSession};
        let catalog = carina_core::parser::LoadedProviderContext::empty_for_test();
        let checks = TypeCheckSession::run(&catalog, |session| {
            session.record(
                &carina_core::parser::TypeExpr::String,
                ProviderTypeCheckSite::at_line(Path::new("main.crn"), 3, 4, "lint check"),
            );
            Err(AppError::Validation("producer failed".into()))
        });
        let error = LintResult::settle(checks)
            .publish(Path::new("main.crn"), crate::output_style::ColorMode::Plain)
            .unwrap_err();
        assert!(error.to_string().contains("producer failed"), "{error}");
        assert!(error.to_string().contains("fully loaded"), "{error}");
    }

    #[test]
    fn lint_loaded_pending_check_is_an_error_finding() {
        use carina_core::validation::deferred_types::{ProviderTypeCheckSite, TypeCheckSession};
        let catalog = carina_core::parser::LoadedProviderContext::empty_for_test();
        let checks = TypeCheckSession::run(&catalog, |session| {
            session.record(
                &carina_core::parser::TypeExpr::String,
                ProviderTypeCheckSite::at_line(Path::new("main.crn"), 3, 4, "lint check"),
            );
            Ok(Vec::new())
        });
        let result = LintResult::settle(checks);
        let mut sink = LintTypeCheckSink {
            path: Path::new("main.crn"),
            warnings: Vec::new(),
            errors: Vec::new(),
        };
        result.checks.publish(&mut sink).unwrap();
        assert!(sink.warnings.is_empty());
        assert_eq!(sink.errors.len(), 1);
        assert!(sink.errors[0].message.contains("fully loaded"));
    }

    #[test]
    fn lint_providerless_fallback_reports_deferred_type_warnings() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "provider broken { source = 'file:///definitely-missing/broken.wasm' }\nfn f(x: Arn) { x }\nlet a = f('arn:aws:s3:::bucket')\nattributes { x: AwsAccountId = '123' }\n").unwrap();
        let runtime = crate::test_support::TestCommandRuntime::new();
        let error = run_lint(&runtime, fixture.path(), &ProviderContext::default()).unwrap_err();
        assert!(
            matches!(&error, AppError::Validation(message) if message == "Found 4 lint warning(s)."),
            "{error}"
        );
    }

    #[test]
    fn lint_preserves_imported_module_pest_syntax_diagnostic() {
        let runtime = crate::test_support::TestCommandRuntime::new();
        let temp = tempfile::tempdir().unwrap();
        let module_dir = temp.path().join("broken");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(
            temp.path().join("main.crn"),
            "let broken = use { source = './broken' }\nlet instance = broken {}\n",
        )
        .unwrap();
        std::fs::write(module_dir.join("main.crn"), "let =\n").unwrap();

        let error = run_lint(&runtime, temp.path(), &ProviderContext::default())
            .expect_err("invalid imported module syntax must fail lint");
        let rendered = error.to_string();

        assert!(rendered.contains("Module resolution error"), "{rendered}");
        assert!(rendered.contains("Syntax error"), "{rendered}");
        assert!(
            rendered.contains("--> 1:"),
            "missing pest location: {rendered}"
        );
        assert_eq!(
            rendered.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {rendered}"
        );
        assert!(
            rendered.contains("expected"),
            "missing pest expectation: {rendered}"
        );
    }

    #[test]
    fn lint_accepts_a_standalone_module_with_an_argument_dependent_if() {
        let runtime = crate::test_support::TestCommandRuntime::new();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("main.crn"),
            r#"arguments { flag: Bool }
attributes {
  value: String = if flag { "yes" } else { "no" }
}
"#,
        )
        .unwrap();

        run_lint(&runtime, temp.path(), &ProviderContext::default())
            .expect("lint must inspect reusable module templates without publishing them");
    }

    fn parsed_with_uses(uses: Vec<(&str, &str)>, calls: Vec<&str>) -> ParsedFile {
        let mut source = String::new();
        for (alias, path) in uses {
            source.push_str(&format!("let {alias} = use {{ source = '{path}' }}\n"));
        }
        for name in calls {
            source.push_str(&format!("{name} {{}}\n"));
        }
        carina_core::parser::parse_file(
            &source,
            std::path::Path::new("lint_fixture.crn"),
            &ProviderContext::default(),
        )
        .expect("test fixture must parse")
    }

    #[test]
    fn mixed_tag_key_warnings_keep_each_same_key_same_line_file() {
        let majority_file = PathBuf::from("majority.crn");
        let a_file = PathBuf::from("a.crn");
        let b_file = PathBuf::from("b.crn");
        let mut entries = collect_tag_keys_for_file(
            "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n    Owner = \"team\"\n}",
            &majority_file,
        );
        let minority_source = "tags = {\n\n    managed_by = \"carina\"\n}";
        entries.extend(collect_tag_keys_for_file(minority_source, &a_file));
        entries.extend(collect_tag_keys_for_file(minority_source, &b_file));

        let warnings = mixed_tag_key_style_warnings(&entries);
        let locations: Vec<_> = warnings
            .iter()
            .map(|warning| (warning.file.clone(), warning.line))
            .collect();

        assert_eq!(locations, vec![(a_file, 3), (b_file, 3)]);
    }

    #[test]
    fn tag_key_population_deduplicates_two_aliases_to_one_module_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root_file = temp.path().join("main.crn");
        let module_dir = temp.path().join("shared");
        let module_file = module_dir.join("main.crn");
        let root_source =
            "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n    Owner = \"team\"\n}";
        let module_source = "attributes {\n  tags = {\n    managed_by = \"carina\"\n    cost_center = \"infra\"\n  }\n}";
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(&root_file, root_source).unwrap();
        std::fs::write(&module_file, module_source).unwrap();

        let parsed = parsed_with_uses(
            vec![("first", "./shared"), ("second", "./shared")],
            vec!["first", "second"],
        );
        let root_inputs = vec![(root_file, root_source.to_string())];

        let warnings =
            mixed_tag_key_style_warnings_for_population(&root_inputs, &parsed, temp.path());

        assert_eq!(warnings.len(), 2);
        assert!(warnings.iter().all(|warning| warning.file == module_file));
        assert!(
            warnings
                .iter()
                .all(|warning| warning.message.contains("(PascalCase)"))
        );
    }

    #[test]
    fn tag_key_population_deduplicates_self_referential_root_directory() {
        let temp = tempfile::tempdir().unwrap();
        let main_file = temp.path().join("main.crn");
        let db_file = temp.path().join("db.crn");
        let main_source = "tags = {\n    Name = \"app\"\n    Environment = \"prod\"\n}";
        let db_source = "tags = {\n    managed_by = \"carina\"\n}";
        std::fs::write(&main_file, main_source).unwrap();
        std::fs::write(&db_file, db_source).unwrap();

        let parsed = parsed_with_uses(vec![("self_module", ".")], vec!["self_module"]);
        let root_inputs = vec![
            (main_file, main_source.to_string()),
            (db_file.clone(), db_source.to_string()),
        ];

        let warnings =
            mixed_tag_key_style_warnings_for_population(&root_inputs, &parsed, temp.path());

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].file, db_file);
        assert_eq!(warnings[0].line, 2);
    }
}
