//! Provider routing findings use the authored provider operand, or the
//! resource declaration when it omitted a required routing directive.
use crate::parser::{AnalysisTree, Located};
use crate::resource::{BindingAddress, ParserPlaceholder};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderRoutingDiagnostic {
    #[error(
        "directives.provider: `{binding}` is not a known provider instance. Declare it with `let {binding} = provider <kind> {{ ... }}`."
    )]
    UnknownInstance { binding: String },
    #[error(
        "directives.provider: instance `{binding}` has kind `{actual_kind}`, but resource `{provider}.{resource_type}` requires kind `{provider}`"
    )]
    KindMismatch {
        binding: String,
        actual_kind: String,
        provider: String,
        resource_type: String,
    },
    #[error(
        "resource `{provider}.{resource_type}` has no default provider instance for kind `{provider}`: either add instance attributes to the top-level `provider {provider}` block, or set `directives {{ provider = <instance> }}` explicitly."
    )]
    MissingDefault {
        provider: String,
        resource_type: String,
    },
}

pub fn check_provider_instance_routing<B: BindingAddress, D>(
    tree: &AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
) -> Vec<Located<ProviderRoutingDiagnostic>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut out = Vec::new();
    for resource in tree.resources() {
        let id = resource.id();
        let binding = resource
            .directives()
            .and_then(|directives| directives.provider_instance.as_ref());
        let detail = match binding {
            Some(binding) => {
                match tree
                    .providers()
                    .iter()
                    .find(|provider| provider.binding.as_deref() == Some(binding.as_str()))
                {
                    None => Some(ProviderRoutingDiagnostic::UnknownInstance {
                        binding: binding.clone(),
                    }),
                    Some(instance) if instance.name != id.provider => {
                        Some(ProviderRoutingDiagnostic::KindMismatch {
                            binding: binding.clone(),
                            actual_kind: instance.name.clone(),
                            provider: id.provider.clone(),
                            resource_type: id.resource_type.clone(),
                        })
                    }
                    Some(_) => None,
                }
            }
            None if !id.provider.is_empty()
                && tree
                    .providers()
                    .iter()
                    .any(|provider| provider.name == id.provider)
                && !tree
                    .providers()
                    .iter()
                    .any(|provider| provider.name == id.provider && provider.is_default()) =>
            {
                Some(ProviderRoutingDiagnostic::MissingDefault {
                    provider: id.provider.clone(),
                    resource_type: id.resource_type.clone(),
                })
            }
            None => None,
        };
        if let Some(detail) = detail {
            out.push(Located::new(resource.provider_location().clone(), detail));
        }
    }
    out
}
