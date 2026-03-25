pub mod apply;
pub mod destroy;
pub mod fmt;
pub mod lint;
pub mod module;
pub mod plan;
pub mod state;
pub mod validate;

use std::collections::HashSet;
use std::path::Path;

use carina_core::module_resolver;
use carina_core::parser::ParsedFile;

use crate::error::AppError;
use crate::wiring::{
    WiringContext, compute_anonymous_identifiers_with_ctx, resolve_names_with_ctx,
    validate_module_calls, validate_provider_region_with_ctx, validate_resource_ref_types_with_ctx,
    validate_resources_with_ctx,
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
pub fn validate_and_resolve(
    parsed: &mut ParsedFile,
    base_dir: &Path,
    skip_resource_validation: bool,
) -> Result<(), AppError> {
    let ctx = WiringContext::new();

    // Validate provider region
    validate_provider_region_with_ctx(&ctx, parsed)?;

    // Validate module call arguments before expansion
    validate_module_calls(parsed, base_dir)?;

    // Resolve module imports and expand module calls
    module_resolver::resolve_modules(parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Resolve names (let bindings -> resource names)
    resolve_names_with_ctx(&ctx, &mut parsed.resources)?;

    if !skip_resource_validation {
        validate_resources_with_ctx(&ctx, &parsed.resources)?;
        let argument_names: HashSet<String> =
            parsed.arguments.iter().map(|a| a.name.clone()).collect();
        validate_resource_ref_types_with_ctx(&ctx, &parsed.resources, &argument_names)?;
    }

    // Compute anonymous identifiers
    compute_anonymous_identifiers_with_ctx(&ctx, &mut parsed.resources, &parsed.providers)?;

    Ok(())
}
