//! Pest parser for the formatter grammar

use pest::Parser;
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "formatter/carina_fmt.pest"]
pub struct CarinaFmtParser;

/// Error type for format parsing
#[derive(Debug)]
pub struct FormatParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

impl std::fmt::Display for FormatParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Parse error at {}:{}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for FormatParseError {}

impl From<pest::error::Error<Rule>> for FormatParseError {
    fn from(err: pest::error::Error<Rule>) -> Self {
        let (line, column) = match err.line_col {
            pest::error::LineColLocation::Pos((l, c)) => (l, c),
            pest::error::LineColLocation::Span((l, c), _) => (l, c),
        };
        FormatParseError {
            message: err.variant.message().to_string(),
            line,
            column,
        }
    }
}

/// Parse source code for formatting
pub fn parse(source: &str) -> Result<pest::iterators::Pairs<'_, Rule>, FormatParseError> {
    CarinaFmtParser::parse(Rule::file, source).map_err(FormatParseError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_provider() {
        let input = "provider aws {\n    region = aws.Region.ap_northeast_1\n}\n";
        let result = parse(input);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_with_comment() {
        let input = "# Header comment\nprovider aws {}\n";
        let result = parse(input);
        assert!(result.is_ok());
    }
}
