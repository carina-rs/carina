//! Directive syntax and closed-set keys are checked without stopping validation.
use crate::parser::{DirectiveSyntaxIssue, Located, SyntaxInventory};
use crate::provider_text::DiagnosticText;
use crate::resource::{BindingAddress, ResourceDirective};

#[derive(Debug, thiserror::Error)]
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

pub fn resource_directive_findings<B: BindingAddress>(
    syntax: &SyntaxInventory<B>,
) -> Vec<Located<ResourceDirectiveFinding>> {
    let known: Vec<_> = ResourceDirective::ALL
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let mut findings: Vec<_> = syntax
        .directive_keys()
        .iter()
        .filter(|key| ResourceDirective::from_key(key.detail()).is_none())
        .map(|key| {
            let name = key.detail().to_string();
            let suggestion = crate::schema::suggest_similar_name(&name, &known, &[]);
            Located::new(
                key.location().clone(),
                ResourceDirectiveFinding::UnknownKey {
                    name: DiagnosticText::new(name),
                    suggestion,
                },
            )
        })
        .collect();
    findings.extend(syntax.directive_issues().iter().map(|issue| {
        let finding = match issue.detail() {
            DirectiveSyntaxIssue::InvalidEntry => ResourceDirectiveFinding::InvalidEntry,
            DirectiveSyntaxIssue::DuplicateBlock { first_line } => {
                ResourceDirectiveFinding::DuplicateBlock {
                    first_line: *first_line,
                }
            }
        };
        Located::new(issue.location().clone(), finding)
    }));
    findings.sort_by(|left, right| {
        let left = left.location();
        let right = right.location();
        left.file().path().cmp(right.file().path()).then_with(|| {
            (left.span().start_line(), left.span().start_column())
                .cmp(&(right.span().start_line(), right.span().start_column()))
        })
    });
    findings
}
