use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::Serialize;

use carina_core::config_loader::{
    find_crn_files_in_dir, get_base_dir, load_configuration_with_config,
};
use carina_core::lint::find_duplicate_attrs;
use carina_core::parser::{File, ProviderContext, ResourceContext, UpstreamState};

use super::validate_and_resolve_errors;
use crate::error::AppError;
use crate::wiring::check_unused_bindings;

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
pub fn validate_with_factories(
    path: &Path,
    factories: Vec<Box<dyn carina_core::provider::ProviderFactory>>,
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
    let mut parsed = loaded.parsed;
    let base_dir = get_base_dir(path);

    let mut error_reports: Vec<String> = Vec::new();
    error_reports.extend(
        loaded
            .identifier_scope_errors
            .iter()
            .map(ToString::to_string),
    );
    // Surface inference failures from `apply_inference` (#2360 stage 2).
    // Each `Unknown` sentinel in `parsed.export_params` has a paired
    // entry here; reporting the underlying reason gives the user the
    // actionable "type annotation required" guidance.
    error_reports.extend(format_inference_errors(&loaded.inference_errors));

    error_reports.extend(
        super::validate_and_resolve_errors_with_factories(
            &mut parsed,
            base_dir,
            false,
            factories,
            std::collections::HashMap::new(),
        )
        .iter()
        .map(ToString::to_string),
    );

    error_reports
}

/// Test-support twin of [`validate_with_factories`] that returns the
/// resource identifier list `run_validate` would display, instead of
/// the error strings. Runs the identical load + resolve pipeline, then
/// derives the list via `validated_resource_ids` — the exact
/// production display path — so e2e tests assert on what the user
/// actually sees (including deferred-for loop bodies, carina#3121).
///
/// Not used outside test code.
pub fn validated_resource_ids_with_factories(
    path: &Path,
    factories: Vec<Box<dyn carina_core::provider::ProviderFactory>>,
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
    let mut parsed = loaded.parsed;
    let base_dir = get_base_dir(path);

    // Run the same resolve/validate pass as the CLI so the parsed tree
    // (and its `deferred_for_expressions`) is in the post-validation
    // state the display path observes.
    let _ = super::validate_and_resolve_errors_with_factories(
        &mut parsed,
        base_dir,
        false,
        factories,
        std::collections::HashMap::new(),
    );

    validated_resource_ids(&parsed)
}

/// Format `LoadedConfig.inference_errors` into "export '<name>': type
/// annotation required: <reason>" strings via the shared
/// `inference::format_inference_error` helper so the CLI and LSP keep
/// emitting the same wording.
fn format_inference_errors(
    errors: &[(String, carina_core::validation::inference::InferenceError)],
) -> Vec<String> {
    errors
        .iter()
        .map(|(name, err)| carina_core::validation::inference::format_inference_error(name, err))
        .collect()
}

/// The list of resource identifiers `validate` reports, derived from
/// [`File::iter_all_resources`] so it stays in sync with every
/// other resource-walking checker (the unified-walk invariant from
/// `notes/specs/2026-04-19-unify-resource-walk-design.md`).
///
/// A `for` loop whose iterable is unresolved at parse time (e.g. a
/// same-config provider-read attribute, carina#3121) contributes a
/// `DeferredForExpression` whose `template_resource` has a `Pending`
/// name — `resource.id` alone would render as a meaningless
/// trailing-dot string. For those entries we render the loop's
/// placeholder address form (`{resource_type}.{binding_name}[?]`) plus
/// the source `for` header and its location so the user can see the
/// loop body the planner intends to manage instead of it silently
/// vanishing from the count and list. The location suffix also keeps
/// two distinct anonymous loops over the *same* iterable
/// distinguishable (`binding_name` + `header` alone are identical for
/// `for _, opt in cert.dvo { … }` repeated twice). Direct resources
/// render via their `id` unchanged.
fn validated_resource_ids<E>(parsed: &File<E>) -> Vec<String> {
    parsed
        .iter_all_resources()
        .map(|(ctx, resource)| match ctx {
            ResourceContext::Direct => resource.id.to_string(),
            ResourceContext::Deferred(d) => {
                let location = match &d.file {
                    Some(file) => format!("{file}:{}", d.line),
                    None => d.line.to_string(),
                };
                format!(
                    "{}.{}[?] (deferred: {} @ {})",
                    d.resource_type, d.binding_name, d.header, location
                )
            }
        })
        .collect()
}

pub fn run_validate(
    path: &Path,
    json: bool,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let mut parsed = loaded.parsed;

    let base_dir = get_base_dir(path);

    // Collect every static error before reporting, so the user sees
    // them all in one pass instead of fixing one, re-running, and
    // finding the next. See #2102 / #2105.
    let mut error_reports: Vec<String> = Vec::new();
    error_reports.extend(
        loaded
            .identifier_scope_errors
            .iter()
            .map(ToString::to_string),
    );
    error_reports.extend(format_inference_errors(&loaded.inference_errors));
    if let Err(AppError::Validation(msg)) =
        check_upstream_state_sources(base_dir, &parsed.upstream_states)
    {
        error_reports.push(msg);
    }

    parsed.print_warnings();

    if !json {
        println!("{}", "Validating...".cyan());
    }

    error_reports.extend(
        validate_and_resolve_errors(&mut parsed, base_dir, false)
            .iter()
            .map(ToString::to_string),
    );

    if !error_reports.is_empty() {
        return Err(AppError::Validation(error_reports.join("\n")));
    }

    // Check for unused let bindings (warnings, not errors)
    let unused_warnings = check_unused_bindings(&loaded.unresolved_parsed);

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
        let resources = validated_resource_ids(&parsed);
        let output = ValidateOutput {
            status: "ok",
            resource_count: resources.len(),
            resources,
            warnings,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|e| format!("Failed to serialize: {}", e))?
        );
        return Ok(());
    }

    let resource_ids = validated_resource_ids(&parsed);
    println!(
        "{}",
        format!("✓ {} resources validated successfully.", resource_ids.len())
            .green()
            .bold()
    );

    for id in &resource_ids {
        println!("  • {}", id);
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

    /// carina#3121 fix A: the `--json` output must enumerate a `for`
    /// loop body over an unresolved (deferred) iterable, not just the
    /// human-readable list. Builds `ValidateOutput` exactly as the
    /// `if json` branch of `run_validate` does — `resource_count` and
    /// `resources` both derived from `validated_resource_ids` — and
    /// asserts the serialized JSON contains the deferred placeholder
    /// entry and a count that includes it. This pins the `--json` path
    /// the e2e test reaches only through the shared helper.
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
        let parsed =
            carina_core::parser::parse(src, &ProviderContext::default()).expect("fixture parses");
        // The loop is deferred (iterable is a same-config provider-read
        // attribute), so it is not in `parsed.resources` but is in
        // `deferred_for_expressions` — exactly the carina#3121 shape.
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        let resources = validated_resource_ids(&parsed);
        let output = ValidateOutput {
            status: "ok",
            resource_count: resources.len(),
            resources,
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
                s.contains("route53.RecordSet") && s.contains("[?]") && s.contains("(deferred:")
            }),
            "json must list the deferred for-loop body as a placeholder \
             entry; got: {listed:?}"
        );
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: binding.to_string(),
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
