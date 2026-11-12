//! Authored text admitted to a diagnostic after intrinsic secret redaction.

use super::{PreprocessedFile, Rule};
use pest::iterators::Pair;
use std::ops::Range;

use super::protected_regions::SecretOwnedRanges;

/// An authored expression whose secret subtrees have already been removed.
///
/// There is deliberately no constructor or conversion from a string. Parser
/// diagnostics retain this value rather than retaining a raw source excerpt.
///
/// ```compile_fail
/// use carina_core::parser::AuthoredExpressionText;
/// let text: AuthoredExpressionText = "secret('plaintext')".into();
/// ```
#[derive(Clone)]
pub struct AuthoredExpressionText {
    admitted: std::sync::Arc<AdmittedAuthoredText>,
}

#[derive(Clone)]
struct AdmittedAuthoredText {
    text: String,
    redactions: SecretOwnedRanges,
}

impl PartialEq for AuthoredExpressionText {
    fn eq(&self, other: &Self) -> bool {
        self.admitted.text == other.admitted.text
    }
}
impl Eq for AuthoredExpressionText {}
impl PartialEq<str> for AuthoredExpressionText {
    fn eq(&self, other: &str) -> bool {
        self.admitted.text == other
    }
}
impl PartialEq<&str> for AuthoredExpressionText {
    fn eq(&self, other: &&str) -> bool {
        self.admitted.text == *other
    }
}
impl std::ops::Deref for AuthoredExpressionText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AuthoredExpressionText {
    // Materialized values (including map keys) are literal data, not authored
    // expressions. Admit them through the canonical escaped literal grammar.
    pub(in crate::parser) fn literal_value(value: &str) -> Self {
        let encoded = format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"));
        let pair = super::grammar::literal_key_pair(&encoded);
        let literal = super::expressions::string_literal::parse_string_literal(pair.clone())
            .expect("encoded literal keys have no interpolation");
        let mut admitted = Self::redact(pair, None);
        std::sync::Arc::make_mut(&mut admitted.admitted).text = literal;
        admitted
    }

    pub(in crate::parser) fn has_redactions(&self) -> bool {
        self.admitted.redactions.iter().next().is_some()
    }

    pub(in crate::parser) fn with_diagnostic_prefix(mut self, prefix: &str) -> Self {
        let admitted = std::sync::Arc::make_mut(&mut self.admitted);
        admitted.text.insert_str(0, prefix);
        self
    }

    /// A loop header is presentation composed from grammar-owned bindings and
    /// its original iterable pair, not a new file to classify. Public iterables
    /// keep their existing normalized value presentation; authored secret calls
    /// retain the parsed pair's redaction instead of reclassifying that display.
    pub(in crate::parser) fn for_header(
        binding: &super::ForBinding,
        iterable: Pair<'_, Rule>,
        original: &PreprocessedFile,
        normalized: &super::deferred_if::DiagnosticExpressionText,
    ) -> Self {
        let mut iterable = Self::redact(iterable, Some(original));
        let protected = iterable.has_redactions();
        let admitted = std::sync::Arc::make_mut(&mut iterable.admitted);
        let surface = if protected {
            admitted.text.trim_end()
        } else {
            normalized.as_str()
        };
        admitted.text = match binding {
            super::ForBinding::Simple(value) => format!("for {value} in {surface}"),
            super::ForBinding::Indexed(index, value) => {
                format!("for ({index}, {value}) in {surface}")
            }
            super::ForBinding::Map(key, value) => format!("for {key}, {value} in {surface}"),
        };
        iterable
    }

    #[cfg(test)]
    pub(crate) fn test_for_header(source: &str) -> Self {
        let parsed = super::parse(
            source,
            &super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("a header fixture must be a complete parsed file");
        let [deferred] = parsed.parsed_deferred_for_expressions() else {
            panic!("a header fixture must contain one deferred loop");
        };
        deferred.header.clone()
    }

    /// The sole admission seam. Walk grammar nodes, never diagnostic strings.
    /// `original` selects the existing heredoc-aware presentation where the
    /// caller previously used authored rather than preprocessed source.
    pub(in crate::parser) fn redact(
        pair: Pair<'_, Rule>,
        original: Option<&PreprocessedFile>,
    ) -> Self {
        let span = pair.as_span();
        let regions = SecretOwnedRanges::from_pair(pair);
        Self::admit(span.start()..span.end(), regions, |range| match original {
            Some(file) => file.original_slice(range).to_string(),
            None => span.get_input()[range].to_string(),
        })
    }

    fn admit(
        span: Range<usize>,
        regions: SecretOwnedRanges,
        project: impl Fn(Range<usize>) -> String,
    ) -> Self {
        let mut text = String::new();
        let mut cursor = span.start;
        for region in regions.iter() {
            let range = region.owned();
            text.push_str(&project(cursor..range.start));
            text.push_str("secret(<redacted>)");
            cursor = range.end;
        }
        text.push_str(&project(cursor..span.end));
        Self {
            admitted: std::sync::Arc::new(AdmittedAuthoredText {
                text,
                redactions: regions,
            }),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.admitted.text
    }

    pub(crate) fn from_public_failed_range(
        source: super::PublicFailedFileText<'_>,
        range: Range<usize>,
    ) -> Self {
        Self::literal_value(&source.text()[range])
    }

    pub(in crate::parser) fn string_contents(mut self) -> Self {
        let admitted = std::sync::Arc::make_mut(&mut self.admitted);
        admitted.text = admitted.text[1..admitted.text.len() - 1].to_owned();
        self
    }
}

impl std::fmt::Debug for AuthoredExpressionText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthoredExpressionText")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for AuthoredExpressionText {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A parser-owned reason paired with admitted authored text. Renderers cannot
/// substitute an arbitrary string for the expression or recover its payload.
#[derive(Debug)]
pub struct AuthoredExpressionDiagnostic {
    expression: AuthoredExpressionText,
    reason: AuthoredExpressionReason,
    range: Option<super::ParseWarningSpan>,
}

#[derive(Debug)]
enum AuthoredExpressionReason {
    UpstreamSource {
        binding: String,
        without_interpolation: bool,
    },
    IgnoreChanges,
    UnterminatedHeredoc {
        line: usize,
    },
}

impl AuthoredExpressionDiagnostic {
    pub(crate) fn unterminated_heredoc(line: usize, marker: AuthoredExpressionText) -> Self {
        Self {
            range: None,
            expression: marker,
            reason: AuthoredExpressionReason::UnterminatedHeredoc { line },
        }
    }

    pub(super) fn range(&self) -> Option<super::ParseWarningSpan> {
        self.range.clone()
    }

    pub(super) fn column_suffix(&self) -> String {
        self.range.as_ref().map_or_else(String::new, |range| {
            format!(", column {}", range.start_column)
        })
    }

    pub(in crate::parser) fn upstream_source(
        expression: AuthoredExpressionText,
        binding: &str,
        without_interpolation: bool,
    ) -> Self {
        Self {
            expression,
            range: None,
            reason: AuthoredExpressionReason::UpstreamSource {
                binding: binding.to_owned(),
                without_interpolation,
            },
        }
    }
    pub(in crate::parser) fn ignore_changes(expression: AuthoredExpressionText) -> Self {
        Self {
            expression,
            range: None,
            reason: AuthoredExpressionReason::IgnoreChanges,
        }
    }
}

impl std::fmt::Display for AuthoredExpressionDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            AuthoredExpressionReason::UpstreamSource {
                binding,
                without_interpolation,
            } => write!(
                f,
                "upstream_state '{binding}': 'source' must be {}, got: {}",
                if *without_interpolation {
                    "a string literal without interpolation"
                } else {
                    "a string literal"
                },
                self.expression,
            ),
            AuthoredExpressionReason::UnterminatedHeredoc { line } => write!(
                f,
                "Unterminated heredoc at line {line}: closing marker '{}' not found",
                self.expression
            ),
            AuthoredExpressionReason::IgnoreChanges => write!(
                f,
                "directives.ignore_changes: `{}` must be a bare attribute path, not a string literal or computed expression",
                self.expression
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::SyntaxDiagnostic;

    #[test]
    fn pest_diagnostic_types_have_no_source_free_conversion() {
        trait AmbiguousIfFrom<A> {
            fn check() {}
        }
        impl<T> AmbiguousIfFrom<()> for T {}
        struct ViaFrom;
        impl<T: From<pest::error::Error<Rule>>> AmbiguousIfFrom<ViaFrom> for T {}
        let _ = <super::super::ParseError as AmbiguousIfFrom<_>>::check;
        let _ = <SyntaxDiagnostic as AmbiguousIfFrom<_>>::check;
    }
}
