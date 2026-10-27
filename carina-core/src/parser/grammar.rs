use pest::Parser;
use pest::error::LineColLocation;
use pest::iterators::Pairs;
use pest_derive::Parser;

use super::error::ParseError;

#[derive(Parser)]
#[grammar = "parser/carina.pest"]
struct CarinaParser;

/// An owned, heredoc-preprocessed file that can yield pest pairs borrowing its
/// transformed source while retaining the original-line mapping.
pub(crate) struct PreprocessedFile {
    preprocess_result: crate::heredoc::PreprocessResult,
}

impl PreprocessedFile {
    pub(crate) fn parse(&self) -> Result<Pairs<'_, Rule>, ParseError> {
        CarinaParser::parse(Rule::file, &self.preprocess_result.source)
            .map_err(|error| ParseError::Syntax(map_pest_error_lines(error, self)))
    }

    pub(crate) fn original_line(&self, preprocessed_line: usize) -> usize {
        self.preprocess_result
            .line_map
            .get(preprocessed_line.saturating_sub(1))
            .copied()
            .unwrap_or(preprocessed_line)
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
    Ok(PreprocessedFile { preprocess_result })
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
    error.line_col = match error.line_col {
        LineColLocation::Pos((line, column)) => {
            LineColLocation::Pos((preprocessed_file.original_line(line), column))
        }
        LineColLocation::Span((start_line, start_column), (end_line, end_column)) => {
            LineColLocation::Span(
                (preprocessed_file.original_line(start_line), start_column),
                (preprocessed_file.original_line(end_line), end_column),
            )
        }
    };
    error
}
