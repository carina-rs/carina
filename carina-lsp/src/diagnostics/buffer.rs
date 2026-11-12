//! Buffer syntax errors and local style lints. No raw constructor is visible
//! to analysis checks; merged findings must enter `render_merged`.
use super::*;

pub(super) fn render_document_parse_error(doc: &Document) -> Option<Diagnostic> {
    doc.parse_error()
        .map(|error| parse_error_in_document(doc, error))
}

/// Create a `Diagnostic` on a single line with the standard "carina" source.
pub(super) fn protected_parse_error(
    protected: crate::document::ProtectedDocumentDiagnostic,
) -> Diagnostic {
    carina_diagnostic_range(
        protected.range,
        DiagnosticSeverity::ERROR,
        protected.failure.to_string(),
    )
}

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
        doc.scalar_utf16_column(line, column as usize)
            .unwrap_or(column),
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
        let duplicates = carina_core::lint::find_duplicate_attrs_in_source(doc.source());

        duplicates
            .into_iter()
            .map(|dup| {
                // Convert 1-indexed line to 0-indexed
                let line = (dup.line - 1) as u32;
                // Convert the detector's 1-indexed character column to LSP's
                // 0-indexed character offset.
                let col = dup.column.saturating_sub(1) as u32;

                carina_diagnostic_range(
                    Range::new(
                        parser_position(doc, line, col),
                        parser_position(doc, line, dup.end_column.saturating_sub(1) as u32),
                    ),
                    DiagnosticSeverity::WARNING,
                    dup.message(),
                )
            })
            .collect()
    }

    /// Check for direct calls to pipe-preferred functions (info-level).
    pub(super) fn check_pipe_preferred_direct_calls(&self, doc: &Document) -> Vec<Diagnostic> {
        let warnings =
            carina_core::lint::pipe_preferred_direct_call_findings_in_source(doc.source());

        warnings
            .into_iter()
            .map(|finding| {
                let (line, col, end_col) = finding.editor_range();
                let pw = finding.detail();
                carina_diagnostic(
                    line,
                    col,
                    end_col,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Consider using pipe form for '{}': data |> {}(...)",
                        pw.name, pw.name
                    ),
                )
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
        let positions = carina_core::lint_text::LintTextIndex::new(&text);
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
                let col = positions.first_on_line(line_idx, name)? as u32;
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
        let warnings = carina_core::lint::non_snake_case_binding_findings_in_source(doc.source());

        warnings
            .into_iter()
            .map(|finding| {
                let (line, col, end_col) = finding.editor_range();
                let nw = finding.detail();
                carina_diagnostic(
                    line,
                    col,
                    end_col,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Binding '{}' is not snake_case. Use snake_case for binding names (e.g., 'my_resource').",
                        nw.name
                    ),
                )
            })
            .collect()
    }
}

pub(super) fn parse_error_message(error: &ParseError) -> String {
    parse_error_to_diagnostic(error).message
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Located { error, .. } => return parse_error_to_diagnostic(error),
        ParseError::AtElement(finding) => return parse_error_to_diagnostic(finding.detail()),
        _ => {}
    }
    let range = match error {
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos(position)
                | pest::error::LineColLocation::Span(position, _) => position,
            };
            Range::new(
                Position::new(line.saturating_sub(1) as u32, col.saturating_sub(1) as u32),
                Position::new(line.saturating_sub(1) as u32, col as u32),
            )
        }
        _ if error.diagnostic_range().is_some() => {
            let span = error.diagnostic_range().expect("range was checked");
            Range::new(
                Position::new(
                    span.start_line.saturating_sub(1) as u32,
                    span.start_column.saturating_sub(1) as u32,
                ),
                Position::new(
                    span.end_line.saturating_sub(1) as u32,
                    span.end_column.saturating_sub(1) as u32,
                ),
            )
        }
        ParseError::InsideSecret { line }
        | ParseError::SecretInConstraint { line }
        | ParseError::InvalidExpression { line, .. }
        | ParseError::InvalidAuthoredExpression { line, .. }
        | ParseError::Evaluation { line, .. }
        | ParseError::InvalidFloatLiteral { line, .. }
        | ParseError::InterpolationNotAllowedInLiteralString { line }
        | ParseError::UnboundLoopRead {
            line, span: None, ..
        }
        | ParseError::InvalidResourceIdentity { line, .. } => Range::new(
            Position::new(line.saturating_sub(1) as u32, 0),
            Position::new(line.saturating_sub(1) as u32, 100),
        ),
        ParseError::DuplicateBinding { line, .. }
        | ParseError::UndefinedIdentifier {
            line, span: None, ..
        } => Range::new(
            Position::new(line.saturating_sub(1) as u32, 0),
            Position::new(line.saturating_sub(1) as u32, 0),
        ),
        _ => Range::default(),
    };
    let message = match error {
        // Preserve the editor's existing syntax presentation. The JSON report
        // uses the source-free projection of the same parser error.
        ParseError::Syntax(error) => error.to_string(),
        _ => error.diagnostic_message(),
    };
    carina_diagnostic_range(range, DiagnosticSeverity::ERROR, message)
}

#[cfg(test)]
mod tests {
    fn round11_buffer_lint_projection_work(kind: usize) {
        use carina_core::test_work::{Operation, measure};
        {
            let run = |count| {
                let source = match kind {
                    0 => format!(
                        "mock.test.resource {{ name = 'x' {} }}",
                        "key = 1 ".repeat(count + 1)
                    ),
                    1 => (0..count)
                        .map(|n| {
                            format!("for a{n:06} in [] {{ mock.test.resource {{ name = 'x' }} }}\n")
                        })
                        .collect(),
                    _ => format!(
                        "mock.test.resource {{ name = 'x' {} }}",
                        "note = '${a}' ".repeat(count)
                    ),
                };
                let doc = crate::document::Document::new(
                    source,
                    std::sync::Arc::new(carina_core::parser::ProviderContext::new(
                        carina_core::EnvironmentSnapshot::empty(),
                    )),
                );
                let engine = DiagnosticEngine::without_loaded_provider_schemas(
                    carina_core::EnvironmentSnapshot::empty(),
                );
                let (findings, work) = measure(|| match kind {
                    0 => engine
                        .check_duplicate_attrs(&doc.for_diagnostics().expect("public fixture")),
                    1 => engine
                        .check_unused_for_bindings(&doc.for_diagnostics().expect("public fixture")),
                    _ => engine.check_single_quoted_interpolation_warnings(
                        &doc.for_diagnostics().expect("public fixture"),
                    ),
                });
                assert_eq!(findings.len(), count, "kind {kind}: {findings:?}");
                [
                    Operation::LintScanBytes,
                    Operation::LintSearchBytes,
                    Operation::LintIndexStep,
                    Operation::LintPositionBytes,
                ]
                .into_iter()
                .map(|op| work.get(op))
                .sum::<usize>()
            };
            let small = run(64);
            let large = run(128);
            assert!(small > 0);
            assert!(
                large <= 3 * small,
                "buffer projection {kind}: {small} -> {large}"
            );
        }
    }

    #[test]
    fn round11_duplicate_buffer_projection_work_is_linear() {
        round11_buffer_lint_projection_work(0);
    }
    #[test]
    fn round11_unused_for_buffer_projection_work_is_linear() {
        round11_buffer_lint_projection_work(1);
    }
    #[test]
    fn round11_single_quoted_buffer_projection_work_is_linear() {
        round11_buffer_lint_projection_work(2);
    }
    #[test]
    fn round11_merged_lint_span_projection_work_is_linear() {
        use carina_core::test_work::{Operation, measure};
        let run = |count| {
            let source = format!(
                "mock.test.resource {{ name = 'x' {} }}",
                "key = 1 ".repeat(count)
            );
            let span = carina_core::parser::SourceSpanIndex::parse(&source)
                .unwrap()
                .attribute_value_source_spans("name", &carina_core::schema::FieldPath::new(), None)
                .unwrap()
                .value;
            let doc = crate::document::Document::new(
                source,
                std::sync::Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            let (_, work) = measure(|| {
                for _ in 0..count {
                    assert!(
                        crate::diagnostics::parser_span_range(
                            &doc.for_diagnostics().expect("public fixture"),
                            span
                        )
                        .is_some()
                    );
                }
            });
            work.get(Operation::LintPositionBytes)
        };
        let small = run(64);
        let large = run(128);
        assert!(small > 0);
        assert!(large <= 3 * small, "merged-span work: {small} -> {large}");
    }

    #[test]
    fn round11_buffer_lints_match_main_messages_positions_and_order() {
        for source in [
            "mock.test.resource { name = '😀é' key = 1 key = 2 key = 3 }",
            "mock.test.resource { name = 'x' note = '${a}' other = '${b}' }",
            "for key, BadValue in [] { mock.test.resource { name = 'x' } }\nfor other in [] { mock.test.resource { name = 'x' } }",
            "let hidden = secret({ 'private-key' = 'a' 'private-key' = 'b' })\nmock.test.resource { name = 'x' key = 1 key = 2 }",
            "# key = 1 key = 2\nlet text = 'key = 1 key = 2'",
        ] {
            let doc = crate::document::Document::new(
                source.to_owned(),
                std::sync::Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            let engine = DiagnosticEngine::without_loaded_provider_schemas(
                carina_core::EnvironmentSnapshot::empty(),
            );
            let old_position = |line, column| {
                Position::new(
                    line,
                    doc.line_at(line).map_or(column, |text| {
                        position::char_offset_to_utf16_offset(&text, column as usize)
                    }),
                )
            };
            let expected_duplicates = carina_core::lint::find_duplicate_attrs(source)
                .into_iter()
                .map(|dup| {
                    let line = (dup.line - 1) as u32;
                    carina_diagnostic_range(
                        Range::new(
                            old_position(line, (dup.column - 1) as u32),
                            old_position(line, (dup.end_column - 1) as u32),
                        ),
                        DiagnosticSeverity::WARNING,
                        dup.message(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                engine.check_duplicate_attrs(&doc.for_diagnostics().expect("public fixture")),
                expected_duplicates
            );
            if let Some(parsed) = doc.parsed() {
                let expected_unused = parsed
                    .warnings
                    .iter()
                    .filter(|w| w.kind == WarningKind::UnusedForBinding)
                    .filter_map(|w| {
                        let name = w
                            .message
                            .strip_prefix("for-loop binding '")?
                            .split('\'')
                            .next()?;
                        let line = w.line - 1;
                        let text = source.lines().nth(line)?;
                        let byte = text.find(name)?;
                        let col = position::byte_offset_to_char_offset(text, byte);
                        Some(carina_diagnostic(
                            line as u32,
                            col,
                            col + position::char_len(name),
                            DiagnosticSeverity::WARNING,
                            w.message.clone(),
                        ))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    engine
                        .check_unused_for_bindings(&doc.for_diagnostics().expect("public fixture")),
                    expected_unused
                );
                let expected_strings = parsed
                    .warnings
                    .iter()
                    .filter(|w| w.kind == WarningKind::SingleQuotedInterpolation)
                    .filter_map(|w| {
                        let span = w.span.as_ref()?;
                        let line = (span.start_line - 1) as u32;
                        Some(carina_diagnostic_range(
                            Range::new(
                                old_position(line, (span.start_column - 1) as u32),
                                old_position(line, (span.end_column - 1) as u32),
                            ),
                            DiagnosticSeverity::WARNING,
                            w.message.clone(),
                        ))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    engine.check_single_quoted_interpolation_warnings(
                        &doc.for_diagnostics().expect("public fixture")
                    ),
                    expected_strings
                );
            }
        }
    }

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
