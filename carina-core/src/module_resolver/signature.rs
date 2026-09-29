use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;

use crate::parser::{ArgumentParameter, File, ParsedFile, ProviderContext, RequireBlock, TypeExpr};

/// The statically declared boundary of one imported module after dotted type
/// expressions have been resolved against the caller's provider context.
#[derive(Debug, Clone)]
pub struct ResolvedModuleSignature {
    pub arguments: Vec<ArgumentParameter>,
    pub attributes: IndexMap<String, Option<TypeExpr>>,
    pub requires: Vec<RequireBlock>,
}

/// Imported module alias to its resolved argument/output signature.
pub type ResolvedModuleSignatures = HashMap<String, ResolvedModuleSignature>;

// This snapshot loader does not own diagnostics. It keeps an unresolved type
// when best-effort resolution fails so callers can still inspect the imported
// signature; ModuleResolver::load_directory_module later runs
// resolve_file_type_exprs over the imported module itself and rejects the
// authored declaration before expansion. The end-to-end guarantee is pinned
// by `carina-cli/tests/validate_unknown_custom_type_e2e.rs`.
fn resolved_signature(parsed: &ParsedFile, config: &ProviderContext) -> ResolvedModuleSignature {
    let arguments = parsed
        .arguments
        .iter()
        .cloned()
        .map(|mut argument| {
            if let Ok(resolved) = crate::validation::resolve_type_expr(&argument.type_expr, config)
            {
                argument.type_expr = resolved;
            }
            argument
        })
        .collect();
    let attributes = parsed
        .attribute_params
        .iter()
        .map(|attribute| {
            let resolved = attribute.type_expr.as_ref().map(|type_expr| {
                crate::validation::resolve_type_expr(type_expr, config)
                    .unwrap_or_else(|_| type_expr.clone())
            });
            (attribute.name.clone(), resolved)
        })
        .collect();

    ResolvedModuleSignature {
        arguments,
        attributes,
        requires: parsed.requires.clone(),
    }
}

/// Load and resolve every module signature imported by `parsed`.
pub fn load_resolved_module_signatures<E>(
    parsed: &File<E>,
    base_dir: &Path,
    config: &ProviderContext,
) -> ResolvedModuleSignatures {
    parsed
        .uses
        .iter()
        .filter_map(|import| {
            super::load_module(&base_dir.join(&import.path))
                .map(|module| (import.alias.clone(), resolved_signature(&module, config)))
        })
        .collect()
}
