use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;

use crate::parser::{ArgumentParameter, File, ParsedFile, ProviderContext, TypeExpr, UseStatement};

/// The statically declared boundary of one imported module after dotted type
/// expressions have been resolved against the caller's provider context.
#[derive(Debug, Clone)]
pub struct ResolvedModuleSignature {
    pub arguments: Vec<ArgumentParameter>,
    pub attributes: IndexMap<String, Option<TypeExpr>>,
}

/// Imported module alias to its resolved argument/output signature.
pub type ResolvedModuleSignatures = HashMap<String, ResolvedModuleSignature>;

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
    }
}

/// Resolve signatures using a caller-supplied module snapshot loader.
///
/// The callback seam lets CLI recursive validation reuse its existing
/// `ModuleWalk`, while ordinary CLI and LSP validation use the filesystem
/// wrapper below. Type-expression resolution remains identical in both cases.
pub fn resolve_module_signatures_with<E>(
    parsed: &File<E>,
    config: &ProviderContext,
    mut load: impl FnMut(&UseStatement) -> Option<ParsedFile>,
) -> ResolvedModuleSignatures {
    parsed
        .uses
        .iter()
        .filter_map(|import| {
            load(import).map(|module| (import.alias.clone(), resolved_signature(&module, config)))
        })
        .collect()
}

/// Load and resolve every module signature imported by `parsed`.
pub fn load_resolved_module_signatures<E>(
    parsed: &File<E>,
    base_dir: &Path,
    config: &ProviderContext,
) -> ResolvedModuleSignatures {
    resolve_module_signatures_with(parsed, config, |import| {
        super::load_module(&base_dir.join(&import.path))
    })
}
