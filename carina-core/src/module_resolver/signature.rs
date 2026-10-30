use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;

use crate::parser::{ArgumentParameter, File, ParsedFile, ProviderContext, RequireBlock};
use crate::static_type::ConsumerStaticType;

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
// when best-effort resolution fails; the strict module load later rejects
// genuinely invalid authored declarations before expansion. The end-to-end
// guarantee is pinned by
// `carina-cli/tests/validate_unknown_custom_type_e2e.rs`.
pub(super) fn resolved_signature(
    parsed: &ParsedFile,
    config: &ProviderContext,
) -> ResolvedModuleSignature {
    let arguments = parsed
        .arguments
        .iter()
        .cloned()
        .map(|mut argument| {
            argument.static_type = crate::validation::resolve_consumer_static_type_for_signature(
                &argument.static_type,
                config,
            );
            argument
        })
        .collect();
    let attributes = parsed
        .attribute_params
        .iter()
        .map(|attribute| {
            let resolved = attribute.static_type.as_ref().map(|static_type| {
                crate::validation::resolve_consumer_static_type_for_signature(static_type, config)
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
