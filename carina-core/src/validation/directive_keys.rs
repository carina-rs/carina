//! Closed-set directive keys are checked without stopping directory validation.
use crate::parser::{Located, SyntaxInventory};
use crate::provider_text::DiagnosticText;
use crate::resource::{BindingAddress, ResourceDirective};

#[derive(Debug, thiserror::Error)]
#[error("Unknown directive '{name}'{}", suggestion.as_ref().map(|name| format!(", did you mean '{name}'?")).unwrap_or_default())]
pub struct UnknownDirectiveKey {
    name: DiagnosticText,
    suggestion: Option<String>,
}

pub fn unknown_directive_keys<B: BindingAddress>(
    syntax: &SyntaxInventory<B>,
) -> Vec<Located<UnknownDirectiveKey>> {
    let known: Vec<_> = ResourceDirective::ALL
        .iter()
        .map(|(name, _)| *name)
        .collect();
    syntax
        .directive_keys()
        .iter()
        .filter(|key| ResourceDirective::from_key(key.detail()).is_none())
        .map(|key| {
            let name = key.detail().to_string();
            let suggestion = crate::schema::suggest_similar_name(&name, &known, &[]);
            Located::new(
                key.location().clone(),
                UnknownDirectiveKey {
                    name: DiagnosticText::new(name),
                    suggestion,
                },
            )
        })
        .collect()
}
