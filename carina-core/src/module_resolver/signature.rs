use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;

use crate::parser::{ArgumentParameter, File, ParsedFile, ProviderContext, RequireBlock};
use crate::static_type::{ConsumerStaticType, StaticType};

/// The statically declared boundary of one imported module after dotted type
/// expressions have been resolved against the caller's provider context.
#[derive(Debug, Clone)]
pub struct ResolvedModuleSignature {
    pub arguments: Vec<ArgumentParameter>,
    pub attributes: IndexMap<String, Option<ConsumerStaticType>>,
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
pub(super) fn resolved_signature(
    parsed: &ParsedFile,
    config: &ProviderContext,
) -> ResolvedModuleSignature {
    let arguments = parsed
        .arguments
        .iter()
        .cloned()
        .map(|mut argument| {
            match &mut argument.static_type {
                StaticType::Known(type_expr) => {
                    if let Ok(resolved) = crate::validation::resolve_type_expr(type_expr, config) {
                        *type_expr = resolved;
                    }
                }
                StaticType::Unchecked(_reason) => {}
            }
            argument
        })
        .collect();
    let attributes = parsed
        .attribute_params
        .iter()
        .map(|attribute| {
            let resolved = attribute
                .static_type
                .as_ref()
                .map(|static_type| match static_type {
                    StaticType::Known(type_expr) => {
                        match crate::validation::resolve_type_expr(type_expr, config) {
                            Ok(resolved) => StaticType::Known(resolved),
                            Err(_error) => StaticType::Known(type_expr.clone()),
                        }
                    }
                    StaticType::Unchecked(reason) => StaticType::Unchecked(reason.clone()),
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
