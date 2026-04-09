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
use carina_core::parser::{ParsedFile, ProviderContext};

use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, compute_anonymous_identifiers_with_ctx,
    resolve_names_with_ctx, validate_module_calls, validate_provider_region_with_ctx,
    validate_resource_ref_types_with_ctx, validate_resources_with_ctx,
};

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
    validate_and_resolve_with_config(
        parsed,
        base_dir,
        skip_resource_validation,
        &ProviderContext::default(),
    )
}

pub fn validate_and_resolve_with_config(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
    provider_context: &ProviderContext,
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

    // Resolve module imports and expand module calls
    module_resolver::resolve_modules_with_config(parsed, base_dir, provider_context)
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
    use std::collections::{HashMap, HashSet};

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
        let result = validate_and_resolve_with_config(
            &mut parsed,
            base_dir,
            false,
            &ProviderContext::default(),
        );

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
        let result = validate_and_resolve_with_config(
            &mut parsed,
            base_dir,
            false,
            &ProviderContext::default(),
        );

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("has no source configured"),
            "Error should tell user to add source. Got: {}",
            msg
        );
    }
}
