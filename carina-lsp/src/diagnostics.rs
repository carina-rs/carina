use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use carina_core::parser::ParseError;

pub struct DiagnosticEngine;

impl DiagnosticEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn analyze(&self, doc: &Document) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        if let Some(error) = doc.parse_error() {
            diagnostics.push(parse_error_to_diagnostic(error));
        }

        diagnostics
    }
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            Diagnostic {
                range: Range {
                    start: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: (col.saturating_sub(1)) as u32,
                    },
                    end: Position {
                        line: (line.saturating_sub(1)) as u32,
                        character: col as u32,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("carina".to_string()),
                message: format!("{}", pest_error),
                ..Default::default()
            }
        }
        ParseError::InvalidExpression { line, message } => Diagnostic {
            range: Range {
                start: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 0,
                },
                end: Position {
                    line: (*line as u32).saturating_sub(1),
                    character: 100,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: message.clone(),
            ..Default::default()
        },
        ParseError::UndefinedVariable(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Undefined variable: {}", name),
            ..Default::default()
        },
        ParseError::EnvVarNotSet(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("carina".to_string()),
            message: format!("Environment variable not set: {}", name),
            ..Default::default()
        },
        ParseError::InvalidResourceType(name) => Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".to_string()),
            message: format!("Invalid resource type: {}", name),
            ..Default::default()
        },
    }
}
