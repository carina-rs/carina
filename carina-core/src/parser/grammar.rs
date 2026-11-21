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

/// Decode one complete literal attribute key using the runtime parser's rules.
/// Editor features can use the same identity for bare and quoted assignments.
pub fn decode_attribute_key(source: &str) -> Option<String> {
    let pair = if source.starts_with(['\'', '"']) {
        complete_string_pair(source).ok()?
    } else {
        CarinaParser::parse(Rule::identifier, source).ok()?.next()?
    };
    if pair.as_span().start() != 0 || pair.as_span().end() != source.len() {
        return None;
    }
    super::context::extract_key_string(pair).ok()
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
        let pairs = CarinaParser::parse(Rule::file, self.preprocess_result.transformed_source())
            .map_err(|error| {
                let error = map_pest_error_lines(error, self);
                match super::failed_file_text(self.preprocess_result.authored_source()) {
                    super::FailedFileText::Public(_) => ParseError::Syntax(SyntaxDiagnostic(error)),
                    super::FailedFileText::Protected => {
                        ParseError::ProtectedFile(super::ProtectedParseFailure::syntax(&error))
                    }
                }
            })?;
        for statement in pairs
            .clone()
            .flat_map(|file| file.into_inner())
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let member = statement.into_inner().next().expect("statement member");
            if member.as_rule() == Rule::require_statement {
                let keyword = member
                    .into_inner()
                    .next()
                    .expect("require_statement keyword");
                return Err(ParseError::RequireOutsideArguments {
                    span: super::SourceSpan::from_pest(&keyword, self),
                });
            }
        }
        Ok(pairs)
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

/// Keep an anonymous loop's suggested numeric moves consistent with the
/// suggested bound syntax, without changing its authored source or identity.
pub(crate) fn indexed_bound_loop_rewrite(source: &str) -> Option<std::borrow::Cow<'_, str>> {
    let file = preprocess_file(source).ok()?;
    let statement = file.parse().ok().and_then(|mut pairs| {
        pairs
            .next()?
            .into_inner()
            .find(|pair| pair.as_rule() == Rule::statement)
    })?;
    let expression = statement
        .into_inner()
        .next()
        .filter(|pair| pair.as_rule() == Rule::for_expr)?;
    let binding = expression.into_inner().next().expect("for binding grammar");
    let pattern = binding.clone().into_inner().next()?;
    if pattern.as_rule() != Rule::for_simple_binding {
        return Some(source.into());
    }
    let value = pattern.into_inner().next()?;
    let value = value.as_str();
    let range = binding.as_span();
    let mut rewritten = source.to_string();
    rewritten.replace_range(
        file.original_byte(range.start())..file.original_byte(range.end()),
        &format!("(_, {value})"),
    );
    Some(rewritten.into())
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

// Parse only the trivia between two already parsed coalesce operands, so a
// comment containing question marks cannot become the diagnostic anchor.
pub(super) fn coalesce_operator_offset(source: &str) -> usize {
    CarinaParser::parse(Rule::coalesce_separator, source)
        .expect("coalesce operand separator")
        .next()
        .expect("separator")
        .into_inner()
        .find(|part| part.as_rule() == Rule::coalesce_token)
        .expect("coalesce token")
        .as_span()
        .start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misplaced_require_reports_keyword_range() {
        for (source, line, column) in [
            ("require n == 2, 'x'\n", 1, 1),
            ("arguments {}\n  require n == 2, 'x'\n", 2, 3),
            ("let n = 2\nrequire n == 2, 'x'\n", 2, 1),
            ("let text = <<EOF\nhello\nEOF\nrequire true, 'x'\n", 4, 1),
        ] {
            let file = preprocess_file(source).unwrap();
            let error = file.parse().unwrap_err();
            assert_eq!(
                error.diagnostic_message(),
                "`require` must be written inside an `arguments` block"
            );
            let range = error.diagnostic_range().unwrap();
            assert_eq!((range.start_line, range.start_column), (line, column));
            assert_eq!((range.end_line, range.end_column), (line, column + 7));
        }
    }

    #[test]
    fn issue4398_require_keyword_boundary_in_arguments() {
        for statement in ["requiren == 3, 'x'", "requires == 2, 'x'"] {
            let source = format!("arguments {{ n: Int s: Int\n {statement} }}\n");
            let error = preprocess_file(&source).unwrap().parse().unwrap_err();
            assert!(matches!(error, ParseError::Syntax(_)), "{source}: {error}");
        }
    }

    #[test]
    fn issue4398_require_prefixed_top_level_comparisons_are_ordinary_syntax_errors() {
        for source in ["requiren == 3, 'x'\n", "requires == 2, 'x'\n"] {
            let error = preprocess_file(source).unwrap().parse().unwrap_err();
            assert!(matches!(error, ParseError::Syntax(_)), "{source}: {error}");
        }
    }

    #[test]
    fn require_prefixed_statement_names_keep_parsing() {
        for source in ["requirements {}", "required_x.y {}", "requirement {}"] {
            let file = preprocess_file(source).unwrap();
            file.parse()
                .unwrap_or_else(|error| panic!("{source}: {error}"));
        }
    }

    #[test]
    fn incomplete_misplaced_require_keeps_pest_error() {
        let file = preprocess_file("require n == 2\n").unwrap();
        let error = file.parse().unwrap_err();
        assert!(matches!(error, ParseError::Syntax(_)), "{error}");
        assert!(
            error
                .diagnostic_message()
                .starts_with("Syntax error: expected")
        );
    }

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
