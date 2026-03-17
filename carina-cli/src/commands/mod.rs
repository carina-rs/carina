pub mod apply;
pub mod destroy;
pub mod fmt;
pub mod lint;
pub mod module;
pub mod plan;
pub mod state;
pub mod validate;

use std::path::Path;

use carina_core::module_resolver;
use carina_core::parser::ParsedFile;

use crate::error::AppError;
use crate::wiring::{
    compute_anonymous_identifiers, resolve_names, validate_module_calls, validate_provider_region,
    validate_resource_ref_types, validate_resources,
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
    // Validate provider region
    validate_provider_region(parsed)?;

    // Validate module call arguments before expansion
    validate_module_calls(parsed, base_dir)?;

    // Resolve module imports and expand module calls
    module_resolver::resolve_modules(parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Resolve names (let bindings -> resource names)
    resolve_names(&mut parsed.resources)?;

    if !skip_resource_validation {
        validate_resources(&parsed.resources)?;
        validate_resource_ref_types(&parsed.resources)?;
    }

    // Compute anonymous identifiers
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    Ok(())
}
