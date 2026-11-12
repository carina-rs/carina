//! Failed-file text is admitted once. Protected failures retain no authored text.
use super::{ParseWarningSpan, Rule};

/// A public failed file can only be minted by `failed_file_text`.
///
/// ```compile_fail
/// use carina_core::parser::PublicFailedFileText;
/// let _ = PublicFailedFileText("secret('private')");
/// ```
#[derive(Clone, Copy)]
pub struct PublicFailedFileText<'a>(&'a str);
impl<'a> PublicFailedFileText<'a> {
    pub fn text(self) -> &'a str {
        self.0
    }
}

/// The protected arm has no source to accidentally excerpt or lint.
#[derive(Clone, Copy)]
pub enum FailedFileText<'a> {
    Public(PublicFailedFileText<'a>),
    Protected,
}

/// The one failed-file decision. The canonical identifier continuation set is
/// ASCII_ALPHANUMERIC | "_". There is deliberately no left-boundary test.
pub fn failed_file_text(source: &str) -> FailedFileText<'_> {
    let bytes = source.as_bytes();
    if bytes.windows(6).enumerate().any(|(start, word)| {
        word == b"secret"
            && bytes
                .get(start + 6)
                .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != b'_')
    }) {
        FailedFileText::Protected
    } else {
        FailedFileText::Public(PublicFailedFileText(source))
    }
}

impl std::fmt::Debug for FailedFileText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Public(_) => "PublicFailedFileText { .. }",
            Self::Protected => "ProtectedFailedFile",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ParseFailureKind {
    Syntax,
    UnterminatedHeredoc,
    InvalidExpression,
}
impl std::fmt::Display for ParseFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Syntax => "Syntax error",
            Self::UnterminatedHeredoc => "Unterminated heredoc",
            Self::InvalidExpression => "Invalid expression",
        })
    }
}

/// Only static kind and parser coordinates survive a protected failure.
#[derive(Debug, Clone)]
pub struct ProtectedParseFailure {
    kind: ParseFailureKind,
    range: ParseWarningSpan,
}
impl ProtectedParseFailure {
    pub(super) fn expression(start: (usize, usize), end: (usize, usize)) -> Self {
        Self {
            kind: ParseFailureKind::InvalidExpression,
            range: ParseWarningSpan {
                start_line: start.0,
                start_column: start.1,
                end_line: end.0,
                end_column: end.1,
            },
        }
    }
    pub(super) fn syntax(error: &pest::error::Error<Rule>) -> Self {
        let (start, end) = match error.line_col {
            pest::error::LineColLocation::Pos(position) => (position, position),
            pest::error::LineColLocation::Span(start, end) => (start, end),
        };
        Self {
            kind: ParseFailureKind::Syntax,
            range: ParseWarningSpan {
                start_line: start.0,
                start_column: start.1,
                end_line: end.0,
                end_column: end.1,
            },
        }
    }
    pub(crate) fn heredoc(line: usize, column: usize) -> Self {
        Self {
            kind: ParseFailureKind::UnterminatedHeredoc,
            range: ParseWarningSpan {
                start_line: line,
                start_column: column,
                end_line: line,
                end_column: column,
            },
        }
    }
    pub fn range(&self) -> &ParseWarningSpan {
        &self.range
    }
    pub fn kind(&self) -> ParseFailureKind {
        self.kind
    }
}
impl std::fmt::Display for ProtectedParseFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at line {}, column {}",
            self.kind, self.range.start_line, self.range.start_column
        )
    }
}
impl std::error::Error for ProtectedParseFailure {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_file_decision_has_only_the_canonical_right_boundary() {
        for source in [
            "secret",
            "requiresecret(",
            "ifsecret(",
            "insecret(",
            "mysecret(",
            "secret (",
            "secret/*c*/(",
            "# secret rotation",
            "secreté",
            "0secret",
            "'secret'",
            "secret\n",
        ] {
            assert!(
                matches!(failed_file_text(source), FailedFileText::Protected),
                "{source}"
            );
        }
        for source in [
            "secret_name",
            "secretsmanager",
            "Secret",
            "secret0",
            "secretA",
            "nosecrets",
            "",
        ] {
            assert!(
                matches!(failed_file_text(source), FailedFileText::Public(_)),
                "{source}"
            );
        }
    }
}
