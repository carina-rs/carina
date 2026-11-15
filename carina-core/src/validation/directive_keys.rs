//! Located findings retained by the CST resource directive classifier.
use crate::parser::{Located, ParserBindingMetadata};
use crate::provider_text::DiagnosticText;
use crate::resource::BindingAddress;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResourceDirectiveFinding {
    #[error("Unknown directive '{name}'{}", suggestion.as_ref().map(|name| format!(", did you mean '{name}'?")).unwrap_or_default())]
    UnknownKey {
        name: DiagnosticText,
        suggestion: Option<String>,
    },
    #[error("Directive entries must be 'key = value' assignments")]
    InvalidEntry,
    #[error("Duplicate attribute 'directives' (first defined on line {first_line})")]
    DuplicateBlock { first_line: usize },
}

/// Findings produced by the parser's CST directive classification.
pub fn resource_directive_findings<B: BindingAddress>(
    metadata: &ParserBindingMetadata<B>,
) -> &[Located<ResourceDirectiveFinding>] {
    &metadata.directive_findings
}
