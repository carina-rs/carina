//! Buffer syntax errors and local style lints. No raw constructor is visible
//! to analysis checks; merged findings must enter `render_merged`.
use super::*;

pub(super) fn render_document_parse_error(doc: &Document) -> Option<Diagnostic> {
    doc.parse_error()
        .map(|error| parse_error_in_document(doc, error))
}

/// Create a `Diagnostic` on a single line with the standard "carina" source.
fn carina_diagnostic(
    line: u32,
    start_col: u32,
    end_col: u32,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: start_col,
            },
            end: Position {
                line,
                character: end_col,
            },
        },
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

/// Create a `Diagnostic` with an arbitrary `Range` and the standard "carina" source.
fn carina_diagnostic_range(
    range: Range,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

fn parser_position(doc: &Document, line: u32, column: u32) -> Position {
    Position::new(
        line,
        doc.line_at(line).map_or(column, |source| {
            position::char_offset_to_utf16_offset(&source, column as usize)
        }),
    )
}

fn parse_error_in_document(doc: &Document, error: &ParseError) -> Diagnostic {
    let mut diagnostic = parse_error_to_diagnostic(error);
    diagnostic.range = Range::new(
        parser_position(
            doc,
            diagnostic.range.start.line,
            diagnostic.range.start.character,
        ),
        parser_position(
            doc,
            diagnostic.range.end.line,
            diagnostic.range.end.character,
        ),
    );
    diagnostic
}

impl DiagnosticEngine {
    /// Check for duplicate attribute keys within the same block.
    pub(super) fn check_duplicate_attrs(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let duplicates = carina_core::lint::find_duplicate_attrs(&text);

        duplicates
            .into_iter()
            .map(|dup| {
                // Convert 1-indexed line to 0-indexed
                let line = (dup.line - 1) as u32;
                // Convert the detector's 1-indexed character column to LSP's
                // 0-indexed character offset.
                let col = dup.column.saturating_sub(1) as u32;

                carina_diagnostic_range(
                    Range::new(parser_position(doc, line, col), parser_position(doc, line, dup.end_column.saturating_sub(1) as u32)),
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Duplicate attribute '{}' (first defined on line {}). The last value will be used.",
                        dup.name, dup.first_line
                    ),
                )
            })
            .collect()
    }

    /// Check for direct calls to pipe-preferred functions (info-level).
    pub(super) fn check_pipe_preferred_direct_calls(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_pipe_preferred_direct_calls(&text);

        warnings
            .into_iter()
            .filter_map(|pw| {
                let line = (pw.line - 1) as u32;
                let line_text = text.lines().nth(pw.line - 1)?;
                let pattern = format!("{}(", pw.name);
                let byte_pos = line_text.find(&pattern)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + pw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Consider using pipe form for '{}': data |> {}(...)",
                        pw.name, pw.name
                    ),
                ))
            })
            .collect()
    }

    /// Elevate "for-loop binding '<name>' is unused" parse warnings to LSP
    /// diagnostics. Other ParseWarnings are intentionally not surfaced here:
    /// informational notes (e.g. upstream_state "validate does not inspect")
    /// belong on the CLI, not as editor squiggles.
    pub(super) fn check_unused_for_bindings(&self, doc: &Document) -> Vec<Diagnostic> {
        let Some(parsed) = doc.parsed() else {
            return Vec::new();
        };
        let prefix = "for-loop binding '";
        let text = doc.text();
        parsed
            .warnings
            .iter()
            .filter_map(|w| {
                if w.kind != WarningKind::UnusedForBinding {
                    return None;
                }
                let rest = w.message.strip_prefix(prefix)?;
                let name = rest.split('\'').next()?;
                let line_idx = w.line.checked_sub(1)?;
                let line_text = text.lines().nth(line_idx)?;
                let byte_pos = line_text.find(name)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);
                Some(carina_diagnostic(
                    line_idx as u32,
                    col,
                    col + position::char_len(name),
                    DiagnosticSeverity::WARNING,
                    w.message.clone(),
                ))
            })
            .collect()
    }

    pub(super) fn check_single_quoted_interpolation_warnings(
        &self,
        doc: &Document,
    ) -> Vec<Diagnostic> {
        let Some(parsed) = doc.parsed() else {
            return Vec::new();
        };
        parsed
            .warnings
            .iter()
            .filter(|w| w.kind == WarningKind::SingleQuotedInterpolation)
            .filter_map(|w| {
                let span = w.span.as_ref()?;
                let line = span.start_line.checked_sub(1)? as u32;
                let start_col = span.start_column.checked_sub(1)? as u32;
                let end_col = span.end_column.checked_sub(1)? as u32;
                Some(carina_diagnostic_range(
                    Range::new(
                        parser_position(doc, line, start_col),
                        parser_position(doc, line, end_col),
                    ),
                    DiagnosticSeverity::WARNING,
                    w.message.clone(),
                ))
            })
            .collect()
    }

    /// Check for non-snake_case binding names (info-level).
    pub(super) fn check_non_snake_case_bindings(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_non_snake_case_bindings(&text);

        warnings
            .into_iter()
            .filter_map(|nw| {
                let line = (nw.line - 1) as u32;
                let line_text = text.lines().nth(nw.line - 1)?;
                // Find the binding name position in the line
                let byte_pos = line_text.find(&nw.name)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + nw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Binding '{}' is not snake_case. Use snake_case for binding names (e.g., 'my_resource').",
                        nw.name
                    ),
                ))
            })
            .collect()
    }
}

pub(super) fn parse_error_message(error: &ParseError) -> String {
    parse_error_to_diagnostic(error).message
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Located { error, .. } => parse_error_to_diagnostic(error),
        ParseError::AtElement(finding) => parse_error_to_diagnostic(finding.detail()),
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            carina_diagnostic(
                (line.saturating_sub(1)) as u32,
                (col.saturating_sub(1)) as u32,
                col as u32,
                DiagnosticSeverity::ERROR,
                format!("{}", pest_error),
            )
        }
        ParseError::InsideSecret { line } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            0,
            100,
            DiagnosticSeverity::ERROR,
            carina_core::resource::INSIDE_SECRET_MESSAGE.to_string(),
        ),
        error @ ParseError::SecretInConstraint { line } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            0,
            100,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::InvalidExpression { line, message } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            message.clone(),
        ),
        ParseError::Evaluation { line, error } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::InvalidFloatLiteral { line, source } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            format!(
                "Invalid float literal: {}",
                carina_core::executor::render_error_chain(source)
            ),
        ),
        ParseError::IntegerArithmetic {
            line,
            column,
            end_column,
            message,
        } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            column.saturating_sub(1) as u32,
            end_column.saturating_sub(1) as u32,
            DiagnosticSeverity::ERROR,
            message.clone(),
        ),
        ParseError::UnresolvedDeclarationType {
            token,
            line: _line,
            message,
        } => {
            let span = token.span();
            carina_diagnostic_range(
                Range::new(
                    Position::new(
                        span.start_line().saturating_sub(1) as u32,
                        span.start_column().saturating_sub(1) as u32,
                    ),
                    Position::new(
                        span.end_line().saturating_sub(1) as u32,
                        span.end_column().saturating_sub(1) as u32,
                    ),
                ),
                DiagnosticSeverity::ERROR,
                message.clone(),
            )
        }
        ParseError::UnknownFunction { name, line, column } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            (*column as u32).saturating_sub(1),
            column.saturating_sub(1) as u32 + name.len() as u32,
            DiagnosticSeverity::ERROR,
            format!("Unknown function '{name}'"),
        ),
        ParseError::ReservedResourceAttribute {
            attribute,
            line,
            column,
            end_line,
            end_column,
        } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: line.saturating_sub(1) as u32,
                    character: column.saturating_sub(1) as u32,
                },
                end: Position {
                    line: end_line.saturating_sub(1) as u32,
                    character: end_column.saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            format!(
                "Reserved attribute name '{attribute}': names starting with '_' are reserved for Carina"
            ),
        ),
        ParseError::SecretDerivedBackendAttribute {
            line,
            start_column,
            end_column,
            ..
        } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            start_column.saturating_sub(1) as u32,
            end_column.saturating_sub(1) as u32,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::InterpolationNotAllowedInLiteralString { line } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            "interpolation is not allowed in this string literal".to_string(),
        ),
        ParseError::UnboundLoopRead {
            span: Some(span), ..
        } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: span.start_line().saturating_sub(1) as u32,
                    character: span.start_column().saturating_sub(1) as u32,
                },
                end: Position {
                    line: span.end_line().saturating_sub(1) as u32,
                    character: span.end_column().saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::UnboundLoopRead {
            line, span: None, ..
        } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            0,
            100,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::DeferredForBodyUnsupported(details) => carina_diagnostic_range(
            Range {
                start: Position {
                    line: details.span.start_line().saturating_sub(1) as u32,
                    character: details.span.start_column().saturating_sub(1) as u32,
                },
                end: Position {
                    line: details.span.end_line().saturating_sub(1) as u32,
                    character: details.span.end_column().saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::DeferredForBodyTemplate {
            line: _,
            header: _,
            span,
            error: _,
        } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: span.start_line().saturating_sub(1) as u32,
                    character: span.start_column().saturating_sub(1) as u32,
                },
                end: Position {
                    line: span.end_line().saturating_sub(1) as u32,
                    character: span.end_column().saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::InvalidResourceIdentity { line, error } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::UndefinedVariable(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Undefined variable: {}", name),
        ),
        ParseError::InvalidResourceType(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Invalid resource type: {}", name),
        ),
        ParseError::DuplicateModule(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Duplicate module definition: {}", name),
        ),
        ParseError::DuplicateBinding { name, line } => carina_diagnostic(
            (line - 1) as u32,
            0,
            0,
            DiagnosticSeverity::ERROR,
            format!("Duplicate binding: {}", name),
        ),
        err @ ParseError::UndefinedDeferredIterable { span, .. } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: span.start_line().saturating_sub(1) as u32,
                    character: span.start_column().saturating_sub(1) as u32,
                },
                end: Position {
                    line: span.end_line().saturating_sub(1) as u32,
                    character: span.end_column().saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            err.to_string(),
        ),
        err @ ParseError::UndefinedIdentifier {
            span: Some(span), ..
        } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: span.start_line().saturating_sub(1) as u32,
                    character: span.start_column().saturating_sub(1) as u32,
                },
                end: Position {
                    line: span.end_line().saturating_sub(1) as u32,
                    character: span.end_column().saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            err.to_string(),
        ),
        err @ ParseError::UndefinedIdentifier {
            line, span: None, ..
        } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            0,
            0,
            DiagnosticSeverity::ERROR,
            err.to_string(),
        ),
        ParseError::ModuleNotFound(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Module not found: {}", name),
        ),
        ParseError::InternalError { expected, context } => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!(
                "Internal parser error: expected {} in {}",
                expected, context
            ),
        ),
        ParseError::RecursiveFunction(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Recursive function call detected: {}", name),
        ),
        err @ ParseError::CyclicLetBinding { .. } => {
            carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, err.to_string())
        }
        ParseError::UserFunctionError(msg) => {
            carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, msg.to_string())
        }
        ParseError::UnavailableSecret(error) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_evaluation_error_preserves_the_existing_parse_diagnostic() {
        let cause = carina_core::resource::knowledge::KnowledgeError::<
            carina_core::resource::AbsoluteBindingAddress,
        >::InvalidStage {
            operation: "known-value projection",
        };
        let typed = carina_core::parser::ParseError::Evaluation {
            line: 7,
            error: Box::new(cause.into()),
        };
        let original = carina_core::parser::ParseError::InvalidExpression {
            line: 7,
            message: "known-value projection is not valid at this resolution boundary".into(),
        };
        assert_eq!(
            super::parse_error_to_diagnostic(&typed),
            super::parse_error_to_diagnostic(&original)
        );
    }
    use super::*;
    #[test]
    fn overflowing_float_diagnostics_point_at_the_literal_token() {
        let literal = format!("1{}.0", "0".repeat(400));
        let context =
            carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        for sign in ["", "-"] {
            for (source, line) in [
                (format!("\n\nlet ratio = {sign}{literal}\n"), 2),
                (
                    format!(
                        "arguments {{\nratio: Float {{\nvalidation {{\ncondition = ratio < {sign}{literal}\nerror_message = \"too large\"\n}}\n}}\n}}\n"
                    ),
                    3,
                ),
                (
                    format!(
                        "let source = mock.test.Thing {{ name = 'source' }}\nlet ready = wait source {{\nuntil = source.ratio ==\n{sign}{literal}\n}}\n"
                    ),
                    3,
                ),
            ] {
                let error = carina_core::parser::parse_file(
                    &source,
                    std::path::Path::new("overflow.crn"),
                    &context,
                )
                .unwrap_err();
                let diagnostic = super::parse_error_to_diagnostic(&error);
                assert_eq!(diagnostic.range.start.line, line);
                assert!(
                    diagnostic.message.contains("float"),
                    "{}",
                    diagnostic.message
                );
                assert!(diagnostic.message.contains("inf"), "{}", diagnostic.message);
            }
        }
    }

    #[test]
    fn carina_diagnostic_helper_produces_correct_fields() {
        use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

        let diag = carina_diagnostic(
            5,
            10,
            20,
            DiagnosticSeverity::ERROR,
            "test error message".to_string(),
        );

        assert_eq!(
            diag.range,
            Range {
                start: Position {
                    line: 5,
                    character: 10,
                },
                end: Position {
                    line: 5,
                    character: 20,
                },
            }
        );
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diag.source, Some("carina".to_string()));
        assert_eq!(diag.message, "test error message");
        // Verify default fields are not set
        assert!(diag.code.is_none());
        assert!(diag.code_description.is_none());
        assert!(diag.tags.is_none());
        assert!(diag.related_information.is_none());
        assert!(diag.data.is_none());
    }

    #[test]
    fn carina_diagnostic_helper_with_multiline_range() {
        use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

        let diag = carina_diagnostic_range(
            Range {
                start: Position {
                    line: 3,
                    character: 0,
                },
                end: Position {
                    line: 4,
                    character: 0,
                },
            },
            DiagnosticSeverity::ERROR,
            "resource-level error".to_string(),
        );

        assert_eq!(diag.range.start.line, 3);
        assert_eq!(diag.range.end.line, 4);
        assert_eq!(diag.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diag.source, Some("carina".to_string()));
    }
}
