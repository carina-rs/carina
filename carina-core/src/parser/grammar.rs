use pest::Parser;
use pest::error::{InputLocation, LineColLocation};
use pest::iterators::Pairs;
use pest_derive::Parser;
use std::sync::Arc;

use super::error::ParseError;

#[derive(Parser)]
#[grammar = "parser/carina.pest"]
struct CarinaParser;

/// A binding name whose spelling is guaranteed by the parser grammar.
///
/// Construction is limited to grammar-produced let-binding names and the
/// parser's own identifier-shaped synthetic bindings. Consumers can therefore
/// use the value without re-validating it as a resource identity.
pub(super) struct ParserBindingName(String);

impl ParserBindingName {
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

/// An owned, heredoc-preprocessed file that can yield pest pairs borrowing its
/// transformed source while retaining the original-position mapping.
pub(crate) struct PreprocessedFile {
    preprocess_result: Arc<crate::heredoc::PreprocessResult>,
}

impl PreprocessedFile {
    pub(crate) fn parse(&self) -> Result<Pairs<'_, Rule>, ParseError> {
        CarinaParser::parse(Rule::file, &self.preprocess_result.source)
            .map_err(|error| ParseError::Syntax(map_pest_error_lines(error, self)))
    }

    pub(crate) fn original_position(
        &self,
        preprocessed_byte: usize,
    ) -> crate::heredoc::OriginalPosition {
        self.preprocess_result.original_position(preprocessed_byte)
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
        crate::resource::LoopSource::authored(source_file, span, self.original_slice(range), kind)
    }
}

/// Preprocess heredocs and prepare a file for parsing with [`Rule::file`].
///
/// The private transformed source makes raw file parsing unavailable outside
/// this module while allowing returned pairs to borrow from the owned result.
pub(crate) fn preprocess_file(input: &str) -> Result<PreprocessedFile, ParseError> {
    let preprocess_result = crate::heredoc::preprocess_heredocs(input).map_err(|error| {
        let line = match &error {
            crate::heredoc::HeredocError::Unterminated { line, .. } => *line,
        };
        ParseError::InvalidExpression {
            line,
            message: error.to_string(),
        }
    })?;
    Ok(PreprocessedFile {
        preprocess_result: Arc::new(preprocess_result),
    })
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
