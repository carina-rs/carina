use std::collections::HashMap;
use std::path::Path;

use indexmap::IndexMap;

use crate::parser::{ConsumerArgumentParameter, File, ParsedFile, ProviderContext, RequireBlock};
use crate::resource::{ParserPlaceholder, RelativeBindingName};
use crate::static_type::ConsumerStaticType;

use super::resolver::{StrictTypeResolution, TypeResolutionPolicy};

/// The statically declared boundary of one imported module after dotted type
/// expressions have been resolved against the caller's provider context.
#[derive(Debug, Clone)]
pub struct ResolvedModuleSignature {
    pub arguments: Vec<ConsumerArgumentParameter>,
    pub attributes: IndexMap<String, Option<ConsumerStaticType>>,
    pub requires: Vec<RequireBlock<ParserPlaceholder, RelativeBindingName>>,
}

/// Imported module alias to its resolved argument/output signature.
pub type ResolvedModuleSignatures = HashMap<String, ResolvedModuleSignature>;

// This snapshot loader does not own diagnostics. It keeps a typed unchecked
// reason when consumer-side resolution fails; the strict module load later
// rejects genuinely invalid authored declarations before expansion. The
// end-to-end guarantee is pinned by
// `carina-cli/tests/validate_unknown_custom_type_e2e.rs`.
pub(super) fn resolved_signature(
    parsed: &ParsedFile,
    config: &ProviderContext,
) -> ResolvedModuleSignature {
    resolved_signature_with_policy::<StrictTypeResolution>(parsed, config)
}

pub(super) fn resolved_signature_with_policy<P: TypeResolutionPolicy>(
    parsed: &ParsedFile,
    config: &ProviderContext,
) -> ResolvedModuleSignature {
    let arguments = parsed
        .arguments
        .iter()
        .map(|argument| ConsumerArgumentParameter {
            name: argument.name.clone(),
            static_type: P::resolve_static_type(&argument.static_type, config),
            default: argument.default.clone(),
            description: argument.description.clone(),
            validations: argument.validations.clone(),
        })
        .collect();
    let attributes = parsed
        .attribute_params
        .iter()
        .map(|attribute| {
            let resolved = attribute
                .static_type
                .as_ref()
                .map(|type_expr| P::resolve_static_type(type_expr, config));
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
pub fn load_resolved_module_signatures<E, B: crate::resource::BindingAddress, D>(
    parsed: &File<E, crate::resource::ParserPlaceholder, crate::resource::ParserPlaceholder, D, B>,
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
