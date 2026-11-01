use std::fs;
use std::path::{Path, PathBuf};

use crate::output_style::Colorize;
use serde::Serialize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::find_duplicate_attrs;
use carina_core::parser::{ProviderContext, ResourceRef, UpstreamState};
use carina_core::resource::{BindingAddress, ResourceId};

use crate::error::AppError;
use crate::wiring::ProviderLoad;

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

pub(crate) fn print_provider_type_warning(warning: &carina_core::parser::ParseWarning) {
    eprintln!(
        "  ⚠ {}:{}:{}: {}",
        warning.file.as_deref().unwrap_or("<config>"),
        warning.line,
        warning.span.as_ref().map_or(1, |span| span.start_column),
        warning.message
    );
}

/// Test-only entry point for the validate pipeline (#2247).
///
/// Runs the same pipeline `run_validate` runs (parse → resolve →
/// schema-based validation) but with caller-supplied
/// [`ProviderFactory`] instances. This lets e2e tests exercise the
/// full CLI validation path against hand-built schemas without
/// loading a WASM provider plugin — the LSP-side `e2e_typecheck`
/// tests already do the equivalent for `DiagnosticEngine`, and this
/// keeps the CLI side covered without requiring an `#[ignore]`-gated
/// WASM build.
///
/// Returns the human-readable error strings the CLI would print to
/// stderr (one per line of `Validating failed: ...`). Empty `Vec`
/// means validation passed.
///
/// Not used outside test code.
#[cfg(any(test, feature = "test-utils"))]
pub fn validate_with_factories(
    path: &Path,
    factories: Vec<crate::wiring::LoadedProviderFactory>,
) -> Vec<String> {
    let provider_context = ProviderContext::default();
    let loaded = match load_configuration_with_config(
        path,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    ) {
        Ok(l) => l,
        Err(e) => return vec![e.to_string()],
    };
    let parsed = loaded.parsed;
    let base_dir = get_base_dir(path);
    let providers = ProviderLoad::for_test_with_provider_phase(
        &crate::test_support::TestCommandRuntime::new(),
        factories,
        &parsed.providers,
    );

    let mut error_reports: Vec<String> = Vec::new();
    error_reports.extend(
        super::validate_and_resolve_errors(
            &parsed,
            base_dir,
            &providers,
            false,
            &loaded.duplicate_declarations,
        )
        .iter()
        .map(|error| crate::error::format_error_with_source_chain(error)),
    );

    error_reports
}

/// Test-support twin of [`validate_with_factories`] that returns the
/// rendered resource list `run_validate` would display, instead of
/// the error strings. Runs the identical load + resolve pipeline, then
/// derives the list via `validated_entries` and renders each through
/// the same `Display` boundary the CLI uses — so e2e tests assert on
/// exactly what the user sees (including deferred-for loop bodies,
/// carina#3121).
///
/// Not used outside test code.
#[cfg(any(test, feature = "test-utils"))]
pub fn validated_resource_ids_with_factories(
    path: &Path,
    factories: Vec<crate::wiring::LoadedProviderFactory>,
) -> Vec<String> {
    let provider_context = ProviderContext::default();
    let loaded = match load_configuration_with_config(
        path,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    ) {
        Ok(l) => l,
        Err(e) => panic!("fixture failed to load: {e}"),
    };
    let parsed = loaded.parsed;
    let base_dir = get_base_dir(path);
    let providers = ProviderLoad::for_test_with_provider_phase(
        &crate::test_support::TestCommandRuntime::new(),
        factories,
        &parsed.providers,
    );

    // This adapter exercises only the resource-enumeration half of validate.
    // Keep the identical module/name-resolution seam, but do not reject a
    // fixture before enumeration for an unrelated provider-schema diagnostic.
    let parsed = providers
        .validation_report(&parsed, base_dir, true, &loaded.duplicate_declarations)
        .into_result()
        .unwrap_or_else(|errors| panic!("fixture failed validation: {:?}", errors.as_slice()));

    validated_entries(&parsed)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// One entry in `validate`'s resource enumeration, kept as a typed
/// value until the output boundary so a non-address placeholder can
/// never be mistaken for a resolvable resource id, and a not-yet-named
/// resource can never silently render as a trailing-dot string
/// (carina#3121). `Display` is the *only* place an entry becomes text.
enum ValidatedEntry<'a, B: BindingAddress> {
    /// A direct resource whose identity is already assigned — the common
    /// case, renders exactly as its [`ResourceId`].
    Resolved(&'a ResourceId),
    /// A direct resource still missing identity (anonymous,
    /// identity not yet assigned at the point validate
    /// enumerates). Rendering its `ResourceId` directly would emit a
    /// meaningless trailing-dot string; this variant makes the
    /// not-yet-assigned state explicit instead of relying on an empty
    /// identity string.
    PendingDirect(&'a ResourceId),
    /// The body of a `for` loop whose iterable is unresolved at parse
    /// time (e.g. a same-config provider-read attribute). It is a
    /// loop template, not a single resource — never a resolvable
    /// address. The source location keeps two distinct anonymous
    /// loops over the *same* iterable distinguishable (`binding` +
    /// `header` alone are identical for `for _, opt in cert.dvo { … }`
    /// repeated twice).
    DeferredLoop {
        resource_type: &'a str,
        target: &'a carina_core::parser::DeferredForTarget<B>,
        header: &'a str,
        file: Option<&'a str>,
        line: usize,
    },
}

impl<B: BindingAddress> std::fmt::Display for ValidatedEntry<'_, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidatedEntry::Resolved(id) => write!(f, "{id}"),
            ValidatedEntry::PendingDirect(id) => {
                // `id.display_type()` is provider+type without the
                // (empty) name — explicit `<pending>` beats a bare
                // trailing dot.
                write!(f, "{}.<pending>", id.display_type())
            }
            ValidatedEntry::DeferredLoop {
                resource_type,
                target,
                header,
                file,
                line,
            } => {
                let location = match target {
                    carina_core::parser::DeferredForTarget::Bound(_) => match file {
                        Some(file) => format!("{file}:{line}"),
                        None => line.to_string(),
                    },
                    carina_core::parser::DeferredForTarget::Unbound { .. } => target.display_name(),
                };
                match target {
                    carina_core::parser::DeferredForTarget::Bound(target) => write!(
                        f,
                        "{resource_type}.{} (deferred: {header} @ {location})",
                        target.template_address()
                    ),
                    carina_core::parser::DeferredForTarget::Unbound { .. } => {
                        write!(f, "{resource_type}.<pending> (deferred for at {location})")
                    }
                }
            }
        }
    }
}

/// The resources `validate` enumerates, derived from
/// [`carina_core::parser::File::iter_all_resources`] so it stays in sync with every other
/// resource-walking checker (the unified-walk invariant from
/// `notes/specs/2026-04-19-unify-resource-walk-design.md`).
///
/// Returns typed [`ValidatedEntry`] values, not pre-rendered strings:
/// the json `resources` array and the human list both stringify these
/// at their own boundary, so a deferred placeholder is structurally
/// distinct from a resolved id rather than a string that happens to
/// contain `[?]`.
fn validated_entries<E>(
    parsed: &carina_core::parser::ModuleResolvedFile<E>,
) -> Vec<ValidatedEntry<'_, carina_core::resource::AbsoluteBindingAddress>> {
    parsed
        .iter_all_resources()
        // Every call now has a composition so its typed boundary survives
        // expansion, but an unbound call has no address the user can write.
        // Keep those internal nodes available to validation while excluding
        // them from the command's user-facing resource inventory.
        .filter(|rref| {
            !matches!(
                rref,
                ResourceRef::LiveComposition(composition) if composition.binding.is_none()
            )
        })
        .map(|rref| match rref {
            ResourceRef::Deferred {
                resource: _resource,
                deferred: d,
            } => ValidatedEntry::DeferredLoop {
                resource_type: d.resource_type(),
                target: d.target(),
                header: &d.header,
                file: d.file(),
                line: d.line(),
            },
            // Managed / Virtual / DataSource — all direct, classified
            // by whether the id has a resolved identity.
            ResourceRef::Resource(_)
            | ResourceRef::LiveComposition(_)
            | ResourceRef::DataSource(_) => {
                let id = rref.id();
                if id.identity_str().is_some() {
                    ValidatedEntry::Resolved(id)
                } else {
                    ValidatedEntry::PendingDirect(id)
                }
            }
        })
        .collect()
}

pub fn run_validate(
    runtime: &crate::runtime::CommandRuntime,
    path: &Path,
    json: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let color_mode = runtime.color_mode();
    let loaded = match load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    ) {
        Ok(loaded) => loaded,
        Err(error) => {
            return crate::wiring::settle_type_checks(
                crate::wiring::failed_configuration_report(path, error.into()),
                AppError::Validation,
            );
        }
    };
    let parsed = loaded.parsed;

    let base_dir = get_base_dir(path);

    // Collect every static error before reporting, so the user sees
    // them all in one pass instead of fixing one, re-running, and
    // finding the next. See #2102 / #2105.
    let mut error_reports: Vec<String> = Vec::new();
    if let Err(AppError::Validation(msg)) =
        check_upstream_state_sources(base_dir, &parsed.upstream_states)
    {
        error_reports.push(msg);
    }

    let printed_warning_count = parsed.warnings.len();
    parsed.print_warnings();

    if !json {
        println!("{}", "Validating...".cyan(color_mode));
    }

    let (providers, provider_load_error) =
        ProviderLoad::load_reporting(runtime, &parsed.providers, base_dir)?;
    error_reports.extend(provider_load_error.iter().map(ToString::to_string));
    let validation_report =
        providers.validation_report(&parsed, base_dir, false, &loaded.duplicate_declarations);
    // Publish unresolved provider checks even if another validation/load error
    // prevents a resolved file from being returned.
    let (validation_result, schemas) = validation_report.into_parts();
    let parsed = match validation_result {
        Ok(parsed) => {
            parsed.print_warnings_from(printed_warning_count);
            parsed
        }
        Err(errors) => {
            error_reports.extend(
                errors
                    .as_slice()
                    .iter()
                    .map(|error| crate::error::format_error_with_source_chain(error)),
            );
            return Err(AppError::Validation(error_reports.join("\n")));
        }
    };

    if !error_reports.is_empty() {
        return Err(AppError::Validation(error_reports.join("\n")));
    }

    // Check for unused let bindings (warnings, not errors)
    let unused_warnings = carina_core::validation::check_unused_bindings_with_identity_requirements(
        &loaded.authored_bindings,
        schemas,
    );

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
        let entries = validated_entries(&parsed);
        let output = ValidateOutput {
            status: "ok",
            resource_count: entries.len(),
            resources: entries.iter().map(ToString::to_string).collect(),
            warnings,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|e| format!("Failed to serialize: {}", e))?
        );
        return Ok(());
    }

    let entries = validated_entries(&parsed);
    println!(
        "{}",
        format!("✓ {} resources validated successfully.", entries.len())
            .green(color_mode)
            .bold(color_mode)
    );

    for entry in &entries {
        println!("  • {}", entry);
    }

    for binding in &unused_warnings {
        println!(
            "{}",
            format!(
                "⚠ Unused let binding '{}'. Consider using an anonymous resource instead.",
                binding
            )
            .yellow(color_mode)
        );
    }

    for (file_path, message) in &duplicate_warnings {
        println!(
            "{}",
            format!("⚠ {}:{}", file_path.display(), message).yellow(color_mode)
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
    fn validate_failed_provider_reports_deferred_type_checks_as_typed_warnings() {
        let fixture = tempfile::tempdir().unwrap();
        let path = fixture.path().join("main.crn");
        std::fs::write(&path, "provider broken { source = 'file:///definitely-missing/broken.wasm' }\nfn f(x: Arn) { x }\nlet a = f('arn:aws:s3:::bucket')\nattributes {\n  x: AwsAccountId = '123'\n}\n").unwrap();
        let config = ProviderContext::default();
        let loaded = load_configuration_with_config(
            fixture.path(),
            &config,
            &carina_core::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let providers = ProviderLoad::for_test_with_load_errors(
            &crate::test_support::TestCommandRuntime::new(),
            Vec::new(),
            &loaded.parsed.providers,
            std::collections::HashMap::from([(
                "broken".into(),
                "broken provider failed to load".into(),
            )]),
        );
        let report = providers.validation_report(&loaded.parsed, fixture.path(), false, &[]);
        let warnings = report.provider_type_warnings();
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        for (warning, name, line) in [
            (&warnings[0], "Arn", 2),
            (&warnings[1], "Arn", 3),
            (&warnings[2], "AwsAccountId", 5),
        ] {
            assert_eq!(
                warning.kind,
                carina_core::parser::WarningKind::ProviderSchemasNotLoaded
            );
            assert_eq!(warning.file.as_deref(), path.to_str());
            assert_eq!(warning.line, line);
            assert!(warning.message.contains(name), "{warning:?}");
            assert!(
                warning.message.contains("provider schemas were not loaded"),
                "{warning:?}"
            );
        }
        assert!(
            report
                .errors()
                .iter()
                .any(|error| error.to_string().contains("broken provider failed to load"))
        );
        assert!(
            report.into_result().is_err(),
            "a failed provider load keeps validate's failure convention"
        );
        let runtime = crate::test_support::TestCommandRuntime::new();
        assert!(run_validate(&runtime, fixture.path(), false, &config).is_err());
    }

    #[test]
    fn validate_loaded_catalog_reports_unknown_types_without_unloaded_warnings() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("main.crn"), "fn f(x: Arn) { x }\nlet a = f('arn:aws:s3:::bucket')\nattributes { x: AwsAccountId = '123' }\n").unwrap();
        let loaded = load_configuration_with_config(
            fixture.path(),
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let providers = ProviderLoad::for_test_with_provider_phase(
            &crate::test_support::TestCommandRuntime::new(),
            Vec::new(),
            &loaded.parsed.providers,
        );
        let report = providers.validation_report(&loaded.parsed, fixture.path(), false, &[]);
        assert!(report.provider_type_warnings().is_empty());
        for name in ["Arn", "AwsAccountId"] {
            assert!(
                report
                    .errors()
                    .iter()
                    .any(|error| (matches!(error, AppError::TypeResolution(_)) || matches!(error, AppError::ModuleCall(finding) if finding.kind() == carina_core::validation::ModuleCallFindingKind::TypeResolution))
                        && crate::error::format_error_with_source_chain(error)
                            .contains(&format!("unknown custom type '{name}'"))),
                "{:#?}",
                report.errors()
            );
        }
    }

    fn unknown_provider_instance_fixture() -> tempfile::TempDir {
        let fixture = tempfile::tempdir().expect("provider-routing fixture");
        std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("write provider declaration");
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"mock.test.resource {
  name = "example"
  directives { provider = typo }
}
"#,
        )
        .expect("write routed resource");
        fixture
    }

    fn resolve_file_for_test(
        parsed: carina_core::parser::ParsedFile,
    ) -> carina_core::parser::ModuleResolvedParsedFile {
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                carina_core::module_resolver::resolve_modules_providerless(
                    carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    std::path::Path::new("."),
                    session,
                )
                .expect("test fixture crosses module resolution")
            },
        )
    }

    #[test]
    fn multi_file_empty_interpolations_report_each_site_with_context() {
        let fixture = tempfile::tempdir().expect("fixture");
        std::fs::write(
            fixture.path().join("resource.crn"),
            "let broken = mock.test.Thing {\n  value = \"${}\"\n}\n",
        )
        .expect("write resource fixture");
        std::fs::write(
            fixture.path().join("exports.crn"),
            "exports {\n  broken: String = \"${}\"\n}\n",
        )
        .expect("write export fixture");

        let errors = validate_with_factories(fixture.path(), Vec::new());
        let empty_interpolations = errors
            .iter()
            .filter(|error| error.contains("empty interpolation `${}`"))
            .collect::<Vec<_>>();

        assert_eq!(
            empty_interpolations.len(),
            2,
            "one error must be retained for each offending site: {errors:#?}",
        );
        assert!(
            empty_interpolations.iter().any(|error| {
                error.contains("mock.test.Thing.broken: attribute `value`: empty interpolation `${}` — fill in the expression or remove it")
            }),
            "resource context is missing: {empty_interpolations:#?}",
        );
        assert!(
            empty_interpolations.iter().any(|error| {
                error.contains("exports `broken`: empty interpolation `${}` — fill in the expression or remove it")
            }),
            "export context is missing: {empty_interpolations:#?}",
        );
    }

    #[test]
    fn hard_provider_load_error_keeps_unrelated_static_validation_error() {
        let runtime = crate::test_support::TestCommandRuntime::new();
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"
provider mock {
  source = 'file:///definitely-missing/mock.wasm'
}
backend s3 {
  bucket = "state"
  key = "state.json"
  region = true
}
"#,
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("providers.crn"),
            "provider mock { source = 'file:///definitely-missing/mock.wasm' }\n",
        )
        .unwrap();

        let error = run_validate(&runtime, fixture.path(), false, &ProviderContext::default())
            .expect_err("loader and backend validation errors must both fail validation")
            .to_string();

        assert!(
            error.contains("declared more than once with a source"),
            "hard provider load error is missing: {error}"
        );
        assert!(
            error.contains("Invalid attribute 'region' for backend type 's3'"),
            "unrelated backend validation error is missing: {error}"
        );
    }

    #[test]
    fn validate_reports_multifile_unknown_provider_instance_once() {
        let fixture = unknown_provider_instance_fixture();

        let errors = validate_with_factories(fixture.path(), Vec::new());
        let routing_error = "`typo` is not a known provider instance";
        let occurrences = errors
            .iter()
            .map(|error| error.matches(routing_error).count())
            .sum::<usize>();

        assert_eq!(
            occurrences, 1,
            "validate must report the routing error exactly once: {errors:#?}",
        );
    }

    #[test]
    fn plan_validation_reports_multifile_unknown_provider_instance_once() {
        let fixture = unknown_provider_instance_fixture();
        let loaded = load_configuration_with_config(
            fixture.path(),
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load provider-routing fixture");
        let providers = ProviderLoad::for_test_with_provider_phase(
            &crate::test_support::TestCommandRuntime::new(),
            Vec::new(),
            &loaded.parsed.providers,
        );

        let error = match providers.validate_config_pair(
            loaded.parsed,
            loaded.unresolved_parsed,
            fixture.path(),
            false,
            &loaded.duplicate_declarations,
        ) {
            Ok(_) => panic!("plan validation must reject the unknown provider instance"),
            Err(error) => error,
        };
        let rendered = error.to_string();
        let routing_error = "`typo` is not a known provider instance";

        assert_eq!(
            rendered.matches(routing_error).count(),
            1,
            "plan validation must report the routing error exactly once: {rendered}",
        );
    }

    #[test]
    fn validate_pipeline_reports_multifile_undefined_iterable_reference() {
        let fixture = tempfile::tempdir().expect("undefined iterable fixture");
        std::fs::write(fixture.path().join("providers.crn"), "provider mock {}\n")
            .expect("write provider declaration");
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  mock.test.resource {
    name = name
  }
}
"#,
        )
        .expect("write undefined iterable fixture");

        let loaded = load_configuration_with_config(
            fixture.path(),
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load merged undefined iterable fixture");
        let scope_errors = carina_core::parser::check_identifier_scope(&loaded.parsed);
        assert_eq!(
            scope_errors.len(),
            1,
            "the merged parse must contain exactly one scope error: {scope_errors:#?}",
        );
        match &scope_errors[0] {
            carina_core::parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected scope error: {other}"),
        }

        let errors = validate_with_factories(fixture.path(), Vec::new());
        let occurrences = errors
            .iter()
            .filter(|error| error.contains("Undefined identifier `does_not_exist`"))
            .count();

        assert_eq!(
            occurrences, 1,
            "the owning validation pipeline must report the scope error once: {errors:#?}",
        );
    }

    #[test]
    fn validate_pipeline_accepts_cross_file_resource_reference_scope() {
        let fixture = tempfile::tempdir().expect("cross-file reference fixture");
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"provider mock {}
let attach = mock.test.resource {
  name = caller.name
}
"#,
        )
        .expect("write reference consumer");
        std::fs::write(
            fixture.path().join("bindings.crn"),
            "let caller = mock.test.resource { name = 'caller' }\n",
        )
        .expect("write reference producer");

        let errors = validate_with_factories(fixture.path(), Vec::new());

        assert!(
            errors
                .iter()
                .all(|error| !error.contains("Undefined identifier `caller`")),
            "cross-file binding must be in scope in the owning pipeline: {errors:#?}",
        );
    }

    #[test]
    fn validate_pipeline_accepts_cross_file_data_source_reference_scope() {
        let fixture = tempfile::tempdir().expect("cross-file data-source reference fixture");
        std::fs::write(
            fixture.path().join("main.crn"),
            r#"provider mock {}
let observed_names = roles.names
"#,
        )
        .expect("write data-source reference consumer");
        std::fs::write(
            fixture.path().join("bindings.crn"),
            r#"let roles = read mock.iam.Roles {
  name_regex = ".*"
}
"#,
        )
        .expect("write data-source reference producer");

        let errors = validate_with_factories(fixture.path(), Vec::new());

        assert!(
            errors
                .iter()
                .all(|error| !error.contains("Undefined identifier `roles`")),
            "cross-file data-source binding must be in scope in the owning pipeline: {errors:#?}",
        );
    }

    #[test]
    fn test_validate_output_serialization() {
        let output = ValidateOutput {
            status: "ok",
            resource_count: 2,
            resources: vec![
                "aws.s3.Bucket.my-bucket".to_string(),
                "aws.ec2.Vpc.main".to_string(),
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

    #[test]
    fn root_undefined_deferred_iterable_is_reported_exactly_once() {
        let fixture = tempfile::tempdir().expect("undefined iterable fixture");
        let child = fixture.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(
            child.join("main.crn"),
            "arguments { item: String }\nattributes { value = item }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "let child = use { source = './child' }\n\
             let calls = for item in itmes { child { item = item } }\n",
        )
        .unwrap();

        let errors = validate_with_factories(fixture.path(), Vec::new());

        assert_eq!(errors.len(), 1, "unexpected validation errors: {errors:#?}");
        assert!(errors[0].contains("Undefined identifier `itmes`"));
        assert!(!errors[0].contains("cannot use module call"));
    }

    #[test]
    fn root_undefined_deferred_iterable_is_collected_with_pre_expansion_error() {
        let fixture = tempfile::tempdir().expect("combined validation fixture");
        let child = fixture.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(
            child.join("main.crn"),
            "arguments { item: String }\nattributes { value = item }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "backend s3 { bucket = 'state' key = 'state.json' region = true }\n\
             let child = use { source = './child' }\n\
             let calls = for item in itmes { child { item = item } }\n",
        )
        .unwrap();

        let errors = validate_with_factories(fixture.path(), Vec::new());
        let rendered = errors.join("\n");

        assert!(
            rendered.contains("Invalid attribute 'region' for backend type 's3'"),
            "unrelated pre-expansion error is missing: {errors:#?}",
        );
        assert!(
            rendered.contains("Undefined identifier `itmes`"),
            "undefined deferred iterable was lost behind the early return: {errors:#?}",
        );
    }

    #[test]
    fn module_internal_undefined_iterable_in_module_call_body_is_rejected() {
        let fixture = tempfile::tempdir().expect("module undefined iterable fixture");
        let child = fixture.path().join("child");
        let parent = fixture.path().join("parent");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(
            child.join("main.crn"),
            "arguments { item: String }\nattributes { value = item }\n",
        )
        .unwrap();
        std::fs::write(
            parent.join("main.crn"),
            "let child = use { source = '../child' }\n\
             let items = ['one']\n\
             let calls = for item in itmes { child { item = item } }\n\
             attributes { value = 'parent' }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "let parent = use { source = './parent' }\nparent {}\n",
        )
        .unwrap();

        let errors = validate_with_factories(fixture.path(), Vec::new());

        assert_eq!(errors.len(), 1, "unexpected validation errors: {errors:#?}");
        assert!(errors[0].contains("Undefined identifier `itmes`"));
        assert!(errors[0].contains("parent/main.crn"));
        assert!(errors[0].contains("module call in main.crn at line 2"));
    }

    #[test]
    fn module_internal_undefined_iterable_in_managed_resource_body_is_rejected() {
        let fixture = tempfile::tempdir().expect("managed body undefined iterable fixture");
        let parent = fixture.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(
            parent.join("main.crn"),
            "let items = ['one']\n\
             let resources = for item in itmes {\n\
               mock.test.resource { name = item }\n\
             }\n\
             attributes { value = 'parent' }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("main.crn"),
            "let parent = use { source = './parent' }\nparent {}\n",
        )
        .unwrap();

        let errors = validate_with_factories(fixture.path(), Vec::new());

        assert_eq!(errors.len(), 1, "unexpected validation errors: {errors:#?}");
        assert!(errors[0].contains("Undefined identifier `itmes`"));
        assert!(errors[0].contains("parent/main.crn"));
        assert!(errors[0].contains("line 2"));
    }

    /// carina#3121 fix A: the `--json` output must enumerate a `for`
    /// loop body over an unresolved (deferred) iterable, not just the
    /// human-readable list. Builds `ValidateOutput` exactly as the
    /// `if json` branch of `run_validate` does — `resource_count` and
    /// `resources` both derived from `validated_entries` rendered at
    /// the `Display` boundary — and asserts the serialized JSON
    /// contains the deferred placeholder entry and a count that
    /// includes it. This pins the `--json` path the e2e test reaches
    /// only through the shared helper.
    #[test]
    fn json_output_enumerates_deferred_for_body() {
        let src = r#"
            let cert = aws.acm.Certificate {
                domain_name       = "registry.example.com"
                validation_method = "DNS"
            }

            for _, opt in cert.domain_validation_options {
                aws.route53.RecordSet {
                    hosted_zone_id   = "Z123"
                    name             = opt.resource_record.name
                    type             = "CNAME"
                    ttl              = 300
                    resource_records = [opt.resource_record.value]
                }
            }
        "#;
        let parsed = carina_core::parser::parse_file(
            src,
            std::path::Path::new("validate_fixture.crn"),
            &ProviderContext::default(),
        )
        .expect("fixture parses");
        let parsed = carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                carina_core::module_resolver::resolve_modules_providerless(
                    carina_core::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
                    std::path::Path::new("."),
                    session,
                )
                .expect("cross the deferred-body resolution seam")
            },
        );
        // After the resolution seam the same-config provider-read loop is not
        // in `parsed.resources`; its resource-only template is ready for the
        // validate display path in `deferred_for_expressions`.
        assert_eq!(parsed.deferred_for_expressions().len(), 1);

        let entries = validated_entries(&parsed);
        let output = ValidateOutput {
            status: "ok",
            resource_count: entries.len(),
            resources: entries.iter().map(ToString::to_string).collect(),
            warnings: vec![],
        };
        let json = serde_json::to_string(&output).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // cert (direct) + the deferred RecordSet body.
        assert_eq!(value["resource_count"], 2);
        let listed = value["resources"].as_array().unwrap();
        assert!(
            listed
                .iter()
                .any(|r| r.as_str().unwrap().contains("acm.Certificate")),
            "json must list the let-bound certificate; got: {listed:?}"
        );
        assert!(
            listed.iter().any(|r| {
                let s = r.as_str().unwrap();
                s.contains("route53.RecordSet.<pending>")
                    && s.contains("(deferred for at validate_fixture.crn:7)")
            }),
            "json must list the deferred for-loop body as a placeholder \
             entry; got: {listed:?}"
        );
    }

    /// Type-safety guarantee: a direct resource still missing identity
    /// must NOT render as a trailing-dot string
    /// (`aws.s3.Bucket.`). The `ValidatedEntry::PendingDirect` variant
    /// makes the not-yet-named state explicit at the type level
    /// instead of leaking an empty identity string. Guards the [[feedback_type_safety_over_runtime_checks]]
    /// concern raised reviewing carina#3128.
    #[test]
    fn pending_named_direct_resource_does_not_render_trailing_dot() {
        use carina_core::parser::ParsedFile;
        use carina_core::resource::Resource;

        let mut parsed = ParsedFile::default();
        let mut res = Resource::pending("s3.Bucket");
        res.id.provider = "aws".to_string();
        parsed.resources.push(res); // allow: direct — fixture test inspection
        let parsed = resolve_file_for_test(parsed);

        let entries = validated_entries(&parsed);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(entries[0], ValidatedEntry::PendingDirect(_)),
            "a Pending-named direct resource must classify as PendingDirect"
        );

        let rendered = entries[0].to_string();
        assert!(
            !rendered.ends_with('.'),
            "rendered Pending entry must not be a trailing-dot string; got: {rendered:?}"
        );
        assert_eq!(rendered, "aws.s3.Bucket.<pending>");
    }

    #[test]
    fn validation_listing_excludes_unbound_compositions() {
        use carina_core::parser::ParsedFile;
        use carina_core::resource::{
            AuthoredLiveComposition, LiveCompositionSignature, RelativeBindingName, ResourceId,
        };

        let composition =
            |instance: &'static str, binding: Option<&str>| -> AuthoredLiveComposition {
                AuthoredLiveComposition {
                    id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "_virtual", instance,
                    )),
                    signature: LiveCompositionSignature {
                        arguments: indexmap::IndexMap::new(),
                        attributes: indexmap::IndexMap::new(),
                        pending_constraints: Vec::new(),
                    },
                    binding: binding.map(RelativeBindingName::new),
                    dependency_bindings: Default::default(),
                    module_name: "example".to_string(),
                    instance: instance.to_string(),
                    call_path: crate::test_authored_composition_call_path(),
                    quoted_string_attrs: Default::default(),
                }
            };
        let mut parsed = ParsedFile::default();
        parsed
            .compositions
            .push(composition("bound", Some("bound")));
        parsed
            .compositions
            .push(composition("example_deadbeefdeadbeef", None));
        let parsed = resolve_file_for_test(parsed);

        let rendered: Vec<_> = validated_entries(&parsed)
            .into_iter()
            .map(|entry| entry.to_string())
            .collect();

        assert_eq!(rendered, vec!["_virtual.bound"]);
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: carina_core::resource::RelativeBindingName::new(binding),
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
            resources: vec!["aws.s3.Bucket.test".to_string()],
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
