use pest::Parser;
use pest::error::{InputLocation, LineColLocation};
use pest::iterators::Pairs;
use pest_derive::Parser;
use std::sync::Arc;

use super::error::ParseError;

#[derive(Parser)]
#[grammar = "parser/lexical.pest"]
#[grammar = "parser/carina.pest"]
struct CarinaParser;

/// Syntax presentation with parser-owned coordinates and admitted excerpts.
/// Raw pest errors cannot be admitted without their complete source.
///
/// ```compile_fail
/// use carina_core::parser::{Rule, SyntaxDiagnostic};
/// fn without_source(error: pest::error::Error<Rule>) -> SyntaxDiagnostic {
///     error.into()
/// }
/// ```
#[derive(Debug)]
pub struct SyntaxDiagnostic(pest::error::Error<Rule>);

impl std::ops::Deref for SyntaxDiagnostic {
    type Target = pest::error::Error<Rule>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::fmt::Display for SyntaxDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub(super) fn literal_key_pair(source: &str) -> pest::iterators::Pair<'_, Rule> {
    CarinaParser::parse(Rule::string, source)
        .expect("encoded literal key parses")
        .next()
        .unwrap()
}

pub(super) fn complete_string_pair(source: &str) -> Result<pest::iterators::Pair<'_, Rule>, ()> {
    let pair = CarinaParser::parse(Rule::string, source)
        .map_err(|_| ())?
        .next()
        .ok_or(())?;
    if pair.as_span().start() != 0 || pair.as_span().end() != source.len() {
        return Err(());
    }
    Ok(pair)
}

/// The wait parser joins only identifiers and field accesses with dots. Check
/// that same grammar and spelling when admitting a saved enum identifier.
pub(crate) fn is_wait_enum_identifier(source: &str) -> bool {
    let Ok(mut pairs) = CarinaParser::parse(Rule::variable_ref, source) else {
        return false;
    };
    let Some(pair) = pairs
        .next()
        .filter(|pair| pair.as_span().start() == 0 && pair.as_span().end() == source.len())
    else {
        return false;
    };
    let mut names = Vec::new();
    for part in pair.into_inner() {
        match part.as_rule() {
            Rule::identifier => names.push(part.as_str()),
            Rule::field_access => {
                let Some(identifier) = part.into_inner().next() else {
                    return false;
                };
                names.push(identifier.as_str());
            }
            _ => return false,
        }
    }
    names.join(".") == source
}

/// An identifier or binding name whose spelling is guaranteed by the parser grammar.
///
/// Construction is limited to grammar-produced identifiers/binding names and the
/// parser's own identifier-shaped synthetic bindings. Consumers can therefore
/// use the value without re-validating it as a resource identity.
pub(super) struct ParserBindingName(String);

impl ParserBindingName {
    pub(super) fn from_identifier_pair(pair: pest::iterators::Pair<'_, Rule>) -> Self {
        assert_eq!(pair.as_rule(), Rule::identifier);
        Self(pair.as_str().to_owned())
    }

    pub(super) fn from_let_binding_pair(pair: pest::iterators::Pair<'_, Rule>) -> Self {
        assert!(
            matches!(pair.as_rule(), Rule::identifier | Rule::discard_pattern),
            "let-binding grammar must yield an identifier or discard pattern"
        );
        Self(pair.as_str().to_string())
    }

    pub(super) fn integer_operand(line: usize, column: usize) -> Self {
        Self(format!("__integer_operand_{line}_{column}"))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn into_string(self) -> String {
        self.0
    }
}

#[derive(Clone, Copy)]
pub(super) enum ProviderKindAttribute {
    Source,
    Version,
    Revision,
}
impl ProviderKindAttribute {
    fn name(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Version => "version",
            Self::Revision => "revision",
        }
    }
}

/// These semantic errors historically use an empty pest excerpt. Only closed
/// reasons and grammar-owned identifiers enter this presentation path.
pub(super) enum ProviderSyntaxIssue<'a> {
    VersionAndRevision {
        provider: &'a ParserBindingName,
    },
    NamedKindAttribute {
        field: ProviderKindAttribute,
        binding: &'a ParserBindingName,
        provider: &'a ParserBindingName,
    },
}
impl ProviderSyntaxIssue<'_> {
    pub(super) fn into_parse_error(self) -> ParseError {
        let message = match self {
            Self::VersionAndRevision { provider } => format!(
                "Provider '{}': 'version' and 'revision' are mutually exclusive",
                provider.as_str()
            ),
            Self::NamedKindAttribute {
                field,
                binding,
                provider,
            } => format!(
                "'{}' is a kind-level attribute and cannot be set on \
                 a named provider instance '{}'. Move it to the \
                 top-level `provider {}` block.",
                field.name(),
                binding.as_str(),
                provider.as_str(),
            ),
        };
        ParseError::Syntax(SyntaxDiagnostic(pest::error::Error::new_from_pos(
            pest::error::ErrorVariant::CustomError { message },
            pest::Position::from_start(""),
        )))
    }
}

/// An owned, heredoc-preprocessed file that can yield pest pairs borrowing its
/// transformed source while retaining the original-position mapping.
pub(crate) struct PreprocessedFile {
    preprocess_result: Arc<crate::heredoc::PreprocessResult>,
    secret_regions: std::sync::OnceLock<super::protected_regions::SecretOwnedRanges>,
}

impl PreprocessedFile {
    pub(super) fn from_preprocessed(result: crate::heredoc::PreprocessResult) -> Self {
        Self {
            preprocess_result: Arc::new(result),
            secret_regions: std::sync::OnceLock::new(),
        }
    }

    /// Classification needs syntax without constructing (and recursively
    /// redacting) a syntax diagnostic. Never expose the raw pest failure.
    pub(super) fn syntax_for_secret_classification(&self) -> Option<Pairs<'_, Rule>> {
        CarinaParser::parse(Rule::file, self.preprocess_result.transformed_source()).ok()
    }

    pub(crate) fn parse(&self) -> Result<Pairs<'_, Rule>, ParseError> {
        CarinaParser::parse(Rule::file, self.preprocess_result.transformed_source()).map_err(
            |error| {
                let error = map_pest_error_lines(error, self);
                match super::failed_file_text(self.preprocess_result.authored_source()) {
                    super::FailedFileText::Public(_) => ParseError::Syntax(SyntaxDiagnostic(error)),
                    super::FailedFileText::Protected => {
                        ParseError::ProtectedFile(super::ProtectedParseFailure::syntax(&error))
                    }
                }
            },
        )
    }

    pub(super) fn protected_regions(&self) -> &super::protected_regions::SecretOwnedRanges {
        self.secret_regions.get_or_init(|| {
            let pairs = self
                .syntax_for_secret_classification()
                .expect("provenance is collected only from parsed files");
            super::protected_regions::SecretOwnedRanges::from_pairs(pairs)
        })
    }

    pub(crate) fn original_position(
        &self,
        preprocessed_byte: usize,
    ) -> crate::heredoc::OriginalPosition {
        self.preprocess_result.original_position(preprocessed_byte)
    }

    pub(crate) fn original_byte(&self, preprocessed_byte: usize) -> usize {
        self.preprocess_result.original_byte(preprocessed_byte)
    }

    pub(crate) fn original_slice(&self, preprocessed_range: std::ops::Range<usize>) -> &str {
        self.preprocess_result.original_slice(preprocessed_range)
    }

    pub(super) fn shared_position_map(&self) -> Arc<crate::heredoc::PreprocessResult> {
        Arc::clone(&self.preprocess_result)
    }

    pub(crate) fn loop_source(
        &self,
        pair: &pest::iterators::Pair<'_, Rule>,
        kind: crate::resource::LoopKind,
        source_file: &std::path::Path,
    ) -> crate::resource::LoopSource {
        // Pest's implicit whitespace around an optional trailing clause makes
        // an `if` without `else` consume the newline after its closing brace.
        // Provenance and diagnostics describe the authored expression itself,
        // not separator whitespace before the next statement.
        let mut range = crate::parser::SourceSpan::preprocessed_range(pair);
        range.end -= pair.as_str().len() - pair.as_str().trim_end().len();
        let span = crate::parser::SourceSpan::from_preprocessed_range(range.start, range.end, self);
        let text = super::retained::diagnostic_source_text(pair.clone(), self, source_file);
        crate::resource::LoopSource::authored(source_file, span, text, kind)
    }
}

/// Preprocess heredocs and prepare a file for parsing with [`Rule::file`].
///
/// The private transformed source makes raw file parsing unavailable outside
/// this module while allowing returned pairs to borrow from the owned result.
pub(crate) fn preprocess_file(input: &str) -> Result<PreprocessedFile, ParseError> {
    let preprocess_result = crate::heredoc::preprocess_heredocs(input)
        .map_err(crate::heredoc::HeredocError::into_parse_error)?;
    Ok(PreprocessedFile::from_preprocessed(preprocess_result))
}

pub(super) fn parse_type_expr_fragment(
    input: &str,
) -> Result<Pairs<'_, Rule>, pest::error::Error<Rule>> {
    CarinaParser::parse(Rule::type_expr, input)
}

#[cfg(test)]
pub(super) fn parse_rule_for_test(
    rule: Rule,
    input: &str,
) -> Result<Pairs<'_, Rule>, pest::error::Error<Rule>> {
    CarinaParser::parse(rule, input)
}

fn map_pest_error_lines(
    mut error: pest::error::Error<Rule>,
    preprocessed_file: &PreprocessedFile,
) -> pest::error::Error<Rule> {
    error.line_col = match error.location.clone() {
        InputLocation::Pos(byte) => {
            let position = preprocessed_file.original_position(byte);
            LineColLocation::Pos((position.line(), position.column()))
        }
        InputLocation::Span((start, end)) => {
            let start = preprocessed_file.original_position(start);
            let end = preprocessed_file.original_position(end);
            LineColLocation::Span((start.line(), start.column()), (end.line(), end.column()))
        }
    };
    error
}

/// Intrinsic identity is read only from a canonical parsed identifier.
pub(super) fn is_secret_identifier(input: &str) -> bool {
    input == "secret"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round14_syntax_failure_is_admitted_from_its_own_source() {
        let source = "secret('CANARY'";
        let file = preprocess_file(source).unwrap();
        let error = file.parse().unwrap_err();
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("CANARY"), "{text}");
        }
        assert!(error.protected_failure().is_some());
        let public = preprocess_file("let = {").unwrap();
        let public = public.parse().unwrap_err();
        assert!(matches!(public, ParseError::Syntax(_)));
        assert!(public.to_string().contains("let = {"));
    }

    #[test]
    fn round14_preprocess_failure_is_admitted_from_its_own_source() {
        let source = "let hidden = secret(<<CANARY_MARKER\nCANARY\n";
        let error = preprocess_file(source)
            .err()
            .expect("an unfinished heredoc fails preprocessing");
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("CANARY"), "{text}");
        }
        assert!(error.protected_failure().is_some());
    }
}
