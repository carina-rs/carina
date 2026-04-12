pub mod apply;
pub mod destroy;
pub mod docs;
pub mod fmt;
pub mod init;
pub mod lint;
pub mod module;
pub mod plan;
pub mod skills;
pub mod state;
pub mod validate;

use std::collections::HashSet;
use std::path::Path;

use carina_core::module_resolver;
use carina_core::parser::{BackendConfig, ParsedFile, ProviderContext};
use carina_state::BackendLock;
use carina_state::backend::BackendConfig as StateBackendConfig;

use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, compute_anonymous_identifiers_with_ctx,
    resolve_names_with_ctx, validate_module_calls, validate_provider_region_with_ctx,
    validate_resource_ref_types_with_ctx, validate_resources_with_ctx,
};

/// Detect whether the `backend` block in the current configuration has
/// changed since the last run, by comparing against `.carina/backend-lock.json`
/// under `base_dir`.
///
/// - If no lock exists yet, the current config is written as the new lock.
/// - If the lock matches, nothing happens.
/// - If the lock differs and `reconfigure` is `false`, returns an error
///   explaining the change and asking the user to re-run with `--reconfigure`.
/// - If `reconfigure` is `true`, overwrites the lock with the new config.
///
/// When no `backend` block is configured, the implicit local backend is
/// still recorded in the lock. This makes it possible to detect the
/// local → remote transition (user adds a backend block after having
/// run carina against local state) — otherwise the lock would be silently
/// created with the new remote config and the local state abandoned.
pub fn check_backend_lock(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
    reconfigure: bool,
) -> Result<(), AppError> {
    let current = match backend_config {
        Some(config) => {
            let state_config = StateBackendConfig::from(config);
            BackendLock::from_config(&state_config)
        }
        None => BackendLock::local_default(),
    };
    let existing = BackendLock::load(base_dir).map_err(AppError::Backend)?;

    match existing {
        Some(existing) if existing != current => {
            if reconfigure {
                current.save(base_dir).map_err(AppError::Backend)?;
                Ok(())
            } else {
                Err(AppError::Config(format!(
                    "Backend configuration has changed since the last run:\n\n{}\n\n\
                     Changing backend settings can silently redirect Carina at a \
                     different state file, which may cause state loss or drift.\n\n\
                     To preserve existing state, run `carina state migrate` — it \
                     will copy state from the old backend to the new one and \
                     update the backend lock.\n\n\
                     To discard the old state and start fresh with the new backend, \
                     re-run with --reconfigure.",
                    existing.describe_diff(&current)
                )))
            }
        }
        Some(_) => Ok(()),
        // No lock file yet — don't create one here. The lock is created
        // when state is first written (apply/destroy), not on plan/validate.
        None => Ok(()),
    }
}

/// Save the backend lock file for the current configuration.
/// Called after state is successfully written to ensure the lock
/// exists for future backend-change detection.
pub fn ensure_backend_lock(
    base_dir: &Path,
    backend_config: Option<&BackendConfig>,
) -> Result<(), AppError> {
    let lock_path = BackendLock::lock_path(base_dir);
    if lock_path.exists() {
        return Ok(());
    }
    let lock = match backend_config {
        Some(config) => {
            let state_config = StateBackendConfig::from(config);
            BackendLock::from_config(&state_config)
        }
        None => BackendLock::local_default(),
    };
    lock.save(base_dir).map_err(AppError::Backend)
}

/// Run the common validation and module resolution pipeline.
///
/// Steps:
/// 1. Validate provider region
/// 2. Validate module call arguments (before expansion)
/// 3. Resolve module imports and expand module calls
/// 4. Resolve names (let bindings -> resource names)
/// 5. Validate resources (schema checks) -- skipped when `skip_resource_validation` is true
/// 6. Validate resource ref types -- skipped when `skip_resource_validation` is true
/// 7. Compute anonymous identifiers
///
/// `skip_resource_validation` is used by destroy and state refresh, which only need
/// name resolution and identifier computation without full schema validation.
#[allow(dead_code)] // Used by snapshot tests
pub fn validate_and_resolve(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    validate_and_resolve_with_config(parsed, base_dir, skip_resource_validation)
}

/// Create a `ProviderContext` with custom type validators extracted from
/// the already-collected schema map.
fn enrich_provider_context(
    schemas: &std::collections::HashMap<String, carina_core::schema::ResourceSchema>,
) -> ProviderContext {
    ProviderContext {
        decryptor: None,
        validators: carina_core::provider::collect_custom_type_validators(schemas),
    }
}

pub fn validate_and_resolve_with_config(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    let (factories, load_errors) = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);

    // Check for declared providers whose plugins failed to load
    if !skip_resource_validation {
        let mut errors = Vec::new();
        for provider in &parsed.providers {
            let loaded = ctx.factories().iter().any(|f| f.name() == provider.name);
            if !loaded {
                if let Some(reason) = load_errors.get(&provider.name) {
                    errors.push(reason.clone());
                } else if provider.source.is_none() {
                    errors.push(format!(
                        "Provider '{}' has no source configured. Add `source = 'github.com/...'` to the provider block.",
                        provider.name
                    ));
                }
            }
        }
        if !errors.is_empty() {
            return Err(AppError::Validation(errors.join("\n")));
        }
    }

    // Validate provider region
    validate_provider_region_with_ctx(&ctx, parsed)?;

    // Validate module call arguments before expansion
    validate_module_calls(parsed, base_dir)?;

    // Enrich provider context with custom type validators from loaded schemas
    let enriched_context = enrich_provider_context(ctx.schemas());

    // Resolve module imports and expand module calls
    module_resolver::resolve_modules_with_config(parsed, base_dir, &enriched_context)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Resolve names (let bindings -> resource names)
    resolve_names_with_ctx(&ctx, &mut parsed.resources)?;

    if !skip_resource_validation {
        validate_resources_with_ctx(&ctx, &parsed.resources)?;
        let mut argument_names: HashSet<String> =
            parsed.arguments.iter().map(|a| a.name.clone()).collect();
        // Remote state bindings are resolved at plan time, skip type validation
        for rs in &parsed.remote_states {
            argument_names.insert(rs.binding.clone());
        }
        validate_resource_ref_types_with_ctx(&ctx, &parsed.resources, &argument_names)?;
    }

    // Compute anonymous identifiers
    compute_anonymous_identifiers_with_ctx(&ctx, &mut parsed.resources, &parsed.providers)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::parser::ProviderConfig;
    use carina_core::resource::Value;
    use std::collections::{HashMap, HashSet};

    fn s3_backend_config(bucket: &str, region: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert("bucket".to_string(), Value::String(bucket.to_string()));
        attributes.insert("region".to_string(), Value::String(region.to_string()));
        BackendConfig {
            backend_type: "s3".to_string(),
            attributes,
        }
    }

    #[test]
    fn check_backend_lock_does_not_create_lock_on_first_run() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        let result = check_backend_lock(tmp.path(), Some(&config), false);
        assert!(result.is_ok());
        // Lock should NOT be created by check — only by ensure_backend_lock
        assert!(!tmp.path().join(".carina/backend-lock.json").exists());
    }

    #[test]
    fn ensure_backend_lock_creates_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&config)).unwrap();
        assert!(tmp.path().join(".carina/backend-lock.json").exists());
    }

    #[test]
    fn check_backend_lock_passes_when_config_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let config = s3_backend_config("my-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&config)).unwrap();
        let result = check_backend_lock(tmp.path(), Some(&config), false);
        assert!(result.is_ok());
    }

    #[test]
    fn check_backend_lock_blocks_on_bucket_change() {
        let tmp = tempfile::tempdir().unwrap();
        let old = s3_backend_config("old-bucket", "us-east-1");
        let new = s3_backend_config("new-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&old)).unwrap();
        let err = check_backend_lock(tmp.path(), Some(&new), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Backend configuration has changed"));
        assert!(msg.contains("bucket"));
        assert!(msg.contains("old-bucket"));
        assert!(msg.contains("new-bucket"));
        assert!(msg.contains("carina state migrate"));
        assert!(msg.contains("--reconfigure"));
    }

    #[test]
    fn check_backend_lock_accepts_change_with_reconfigure() {
        let tmp = tempfile::tempdir().unwrap();
        let old = s3_backend_config("old-bucket", "us-east-1");
        let new = s3_backend_config("new-bucket", "us-east-1");
        ensure_backend_lock(tmp.path(), Some(&old)).unwrap();
        let result = check_backend_lock(tmp.path(), Some(&new), true);
        assert!(result.is_ok());
        let result2 = check_backend_lock(tmp.path(), Some(&new), false);
        assert!(result2.is_ok());
    }

    #[test]
    fn check_backend_lock_no_lock_no_backend_does_not_create_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = check_backend_lock(tmp.path(), None, false);
        assert!(result.is_ok());
        assert!(!tmp.path().join(".carina/backend-lock.json").exists());
    }

    #[test]
    fn check_backend_lock_blocks_local_to_remote_transition() {
        let tmp = tempfile::tempdir().unwrap();
        // Simulate first apply with no backend (local default)
        ensure_backend_lock(tmp.path(), None).unwrap();
        // Second run: user adds an S3 backend
        let new = s3_backend_config("my-bucket", "us-east-1");
        let err = check_backend_lock(tmp.path(), Some(&new), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Backend configuration has changed"));
        assert!(msg.contains("local"));
        assert!(msg.contains("s3"));
        assert!(msg.contains("--reconfigure"));
    }

    #[test]
    fn check_backend_lock_allows_local_to_remote_with_reconfigure() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_backend_lock(tmp.path(), None).unwrap();
        let new = s3_backend_config("my-bucket", "us-east-1");
        let result = check_backend_lock(tmp.path(), Some(&new), true);
        assert!(result.is_ok());
        // Subsequent run with the new backend should now pass
        let result2 = check_backend_lock(tmp.path(), Some(&new), false);
        assert!(result2.is_ok());
    }

    fn empty_parsed_file() -> ParsedFile {
        ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            arguments: vec![],
            attribute_params: vec![],
            backend: None,
            state_blocks: vec![],
            user_functions: HashMap::new(),
            remote_states: vec![],
            requires: vec![],
            structural_bindings: HashSet::new(),
        }
    }

    #[test]
    fn test_provider_load_error_shows_actual_reason() {
        // When a provider with source fails to load, the error message should
        // contain the actual failure reason, NOT "Run `carina init`".
        // Use an unsupported source format to trigger a load failure without needing Tokio.
        let mut parsed = empty_parsed_file();
        parsed.providers.push(ProviderConfig {
            name: "fakeprovider".to_string(),
            source: Some("badscheme://not-a-valid-source".to_string()),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            version: None,
            revision: None,
        });

        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-test");
        let result = validate_and_resolve_with_config(&mut parsed, base_dir, false);

        let err = result.unwrap_err();
        let msg = err.to_string();
        // Should NOT suggest "carina init" when the actual problem is a load failure
        assert!(
            !msg.contains("Run `carina init`"),
            "Error should not suggest 'carina init' when the actual failure is known. Got: {}",
            msg
        );
        // Should contain the actual failure reason from build_factories_from_providers
        assert!(
            msg.contains("Unsupported source format for provider 'fakeprovider'"),
            "Error should show actual failure reason. Got: {}",
            msg
        );
    }

    #[test]
    fn test_provider_without_source_shows_source_hint() {
        // A provider without source should tell the user to add source config.
        let mut parsed = empty_parsed_file();
        parsed.providers.push(ProviderConfig {
            name: "awscc".to_string(),
            source: None,
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
            version: None,
            revision: None,
        });

        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-test");
        let result = validate_and_resolve_with_config(&mut parsed, base_dir, false);

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no source configured"),
            "Error should tell user to add source. Got: {}",
            msg
        );
    }
}
