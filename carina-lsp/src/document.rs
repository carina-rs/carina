use std::path::PathBuf;
use std::sync::Arc;

use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent};

use carina_core::parser::{ParseError, ParsedFile, ProviderContext, parse_file};

pub struct Document {
    content: Rope,
    source_text: String,
    parsed: Option<ParsedFile>,
    parse_error: Option<ParseError>,
    provider_context: Arc<ProviderContext>,
    source_file: PathBuf,
    version: i32,
}

impl Document {
    #[cfg(test)]
    pub fn new(content: String, provider_context: Arc<ProviderContext>) -> Self {
        Self::new_with_path(content, PathBuf::from("test.crn"), provider_context)
    }

    pub fn new_with_path(
        content: String,
        source_file: PathBuf,
        provider_context: Arc<ProviderContext>,
    ) -> Self {
        let mut doc = Self {
            content: Rope::from_str(&content),
            source_text: content,
            parsed: None,
            parse_error: None,
            provider_context,
            source_file,
            version: 0,
        };
        doc.reparse();
        doc
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn set_version(&mut self, version: i32) {
        self.version = version;
    }

    pub fn apply_change(&mut self, change: TextDocumentContentChangeEvent) {
        match change.range {
            Some(range) => {
                let start_idx = self.position_to_offset(range.start);
                let end_idx = self.position_to_offset(range.end);
                self.content.remove(start_idx..end_idx);
                self.content.insert(start_idx, &change.text);
            }
            None => {
                self.content = Rope::from_str(&change.text);
            }
        }
        self.reparse();
    }

    fn reparse(&mut self) {
        self.source_text = self.content.to_string();
        match parse_file(&self.source_text, &self.source_file, &self.provider_context) {
            Ok(parsed) => {
                self.parsed = Some(parsed);
                self.parse_error = None;
            }
            Err(e) => {
                self.parsed = None;
                self.parse_error = Some(e);
            }
        }
    }

    fn position_to_offset(&self, pos: Position) -> usize {
        let line_count = self.content.len_lines();
        let line_idx = (pos.line as usize).min(line_count.saturating_sub(1));
        let line_start = self.content.line_to_char(line_idx);
        let line = self.content.line(line_idx);
        let col = crate::position::utf16_offset_to_char_offset(&line.to_string(), pos.character)
            .unwrap_or_else(|| line.len_chars());
        line_start + col
    }

    pub fn text(&self) -> String {
        self.source_text.clone()
    }

    #[allow(dead_code)]
    pub fn parsed(&self) -> Option<&ParsedFile> {
        self.parsed.as_ref()
    }

    /// Provider context the document was parsed with. Exposed so
    /// directory-scoped re-parses (hover, completion, etc.) can reuse the
    /// same configuration when they need to merge sibling `.crn` files.
    pub fn provider_context(&self) -> &ProviderContext {
        &self.provider_context
    }

    pub fn source_file(&self) -> &std::path::Path {
        &self.source_file
    }

    pub fn parse_error(&self) -> Option<&ParseError> {
        self.parse_error.as_ref()
    }

    /// Get the line at the given position
    pub fn line_at(&self, line: u32) -> Option<String> {
        let line_idx = line as usize;
        if line_idx < self.content.len_lines() {
            Some(self.content.line(line_idx).to_string())
        } else {
            None
        }
    }

    /// Ropey maintains these scalar/UTF-16 counts in its text index. Reuse
    /// them for diagnostics instead of copying and recounting line prefixes.
    pub(crate) fn scalar_utf16_column(&self, line: u32, column: usize) -> Option<u32> {
        #[cfg(test)]
        carina_core::test_work::record(carina_core::test_work::Operation::LintPositionBytes, 1);
        if line as usize >= self.content.len_lines() {
            return None;
        }
        let line = self.content.line(line as usize);
        let clamped = column.min(line.len_chars());
        Some((column + line.char_to_utf16_cu(clamped) - clamped) as u32)
    }

    pub(crate) fn line_scalar_len(&self, line: u32) -> Option<usize> {
        ((line as usize) < self.content.len_lines())
            .then(|| self.content.line(line as usize).len_chars())
    }

    pub(crate) fn for_diagnostics(
        &self,
    ) -> Result<DiagnosticDocument<'_>, ProtectedDocumentDiagnostic> {
        let source = carina_core::parser::DiagnosticFileText::new(&self.source_text);
        if source.is_protected_failure() {
            let failure = self
                .parse_error()
                .and_then(ParseError::protected_failure)
                .expect("failed syntax/preprocessing is admitted through FailedFileText")
                .clone();
            let span = failure.range();
            let position = |line: usize, column: usize| {
                let line = (line - 1) as u32;
                Position::new(
                    line,
                    self.scalar_utf16_column(line, column - 1)
                        .unwrap_or((column - 1) as u32),
                )
            };
            let range = tower_lsp::lsp_types::Range::new(
                position(span.start_line, span.start_column),
                position(span.end_line, span.end_column),
            );
            Err(ProtectedDocumentDiagnostic { failure, range })
        } else {
            Ok(DiagnosticDocument {
                document: self,
                source,
            })
        }
    }

    /// Get the word at the given position
    pub fn word_at(&self, position: Position) -> Option<String> {
        let line = self.line_at(position.line)?;
        let col = crate::position::utf16_offset_to_char_offset(&line, position.character)?;

        let chars: Vec<char> = line.chars().collect();

        if col > chars.len() {
            return None;
        }

        // Find word boundaries
        let mut start = col;
        while start > 0 && is_word_char(chars.get(start - 1).copied()?) {
            start -= 1;
        }

        let mut end = col;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }

        if start == end {
            return None;
        }

        Some(chars[start..end].iter().collect())
    }
}

/// Only the file-text gate can construct this capability. Its immutable borrow
/// prevents the underlying document changing after the public decision.
pub(crate) struct DiagnosticDocument<'a> {
    document: &'a Document,
    source: carina_core::parser::DiagnosticFileText<'a>,
}
impl DiagnosticDocument<'_> {
    pub(crate) fn source(&self) -> &carina_core::parser::DiagnosticFileText<'_> {
        &self.source
    }
}
impl std::ops::Deref for DiagnosticDocument<'_> {
    type Target = Document;
    fn deref(&self) -> &Document {
        self.document
    }
}
#[derive(Debug)]
pub(crate) struct ProtectedDocumentDiagnostic {
    pub(crate) failure: carina_core::parser::ProtectedParseFailure,
    pub(crate) range: tower_lsp::lsp_types::Range,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round11_indexed_parser_columns_match_main_for_all_line_breaks() {
        for source in [
            "😀é a\r\n日本 é\n",
            "a\rb\u{85}c\u{2028}😀d\u{2029}",
            "",
            "\n",
        ] {
            let doc = Document::new(
                source.to_owned(),
                Arc::new(ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            for line in 0..doc.content.len_lines() as u32 + 1 {
                let text = doc.line_at(line);
                for column in 0..text.as_ref().map_or(3, |line| line.chars().count() + 3) {
                    let expected = text
                        .as_ref()
                        .map(|line| crate::position::char_offset_to_utf16_offset(line, column));
                    assert_eq!(
                        doc.scalar_utf16_column(line, column),
                        expected,
                        "{source:?}, {line}:{column}"
                    );
                }
            }
        }
    }

    #[test]
    fn analysis_invariant_is_one_diagnostic_and_does_not_panic() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sibling.crn");
        let source = "let item = mock.test.resource { name = 'live' }\n";
        std::fs::write(&path, source).unwrap();
        std::fs::write(directory.path().join("main.crn"), "invalid {{{").unwrap();
        let mut document = Document::new_with_path(
            source.to_string(),
            path.clone(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        document.parsed.as_mut().unwrap().resources[0].declaration = Default::default();
        let diagnostics = crate::diagnostics::DiagnosticEngine::without_loaded_provider_schemas(
            carina_core::EnvironmentSnapshot::empty(),
        )
        .analyze(&document, Some(&path));
        let failures: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic
                    .message
                    .contains("missing declaration provenance")
            })
            .collect();
        assert_eq!(failures.len(), 1, "{diagnostics:#?}");
        assert_eq!(failures[0].range, tower_lsp::lsp_types::Range::default());
    }

    #[test]
    fn named_analysis_invariant_belongs_only_to_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let owner = directory.path().join("sibling.crn");
        let other = directory.path().join("main.crn");
        let source = "let item = mock.test.resource { name = 'live' }\n";
        std::fs::write(&owner, source).unwrap();
        std::fs::write(&other, "let unrelated = 'main'\n").unwrap();
        std::fs::write(directory.path().join("broken.crn"), "invalid {{{").unwrap();
        let context = Arc::new(ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        ));
        let mut document =
            Document::new_with_path(source.to_string(), owner.clone(), context.clone());
        document.parsed.as_mut().unwrap().resources[0]
            .attributes
            .insert(
                "missing_provenance".into(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("injected".into()),
                ),
            );
        let mut other_document =
            Document::new_with_path("let unrelated = 'main'\n".into(), other.clone(), context);
        other_document.parsed = document.parsed.clone();
        let engine = crate::diagnostics::DiagnosticEngine::without_loaded_provider_schemas(
            carina_core::EnvironmentSnapshot::empty(),
        );
        for (document, path, expected) in [(&other_document, &other, 0), (&document, &owner, 1)] {
            let diagnostics = engine.analyze(document, Some(path));
            let findings: Vec<_> = diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.message.contains("missing")
                        && diagnostic.message.contains("provenance")
                })
                .collect();
            assert_eq!(findings.len(), expected, "{diagnostics:#?}");
        }
    }

    #[test]
    fn test_position_to_offset_beyond_last_line() {
        let mut doc = Document::new(
            "hello\nworld".to_string(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        // Line 10 is beyond the document - should not panic
        let change = TextDocumentContentChangeEvent {
            range: Some(tower_lsp::lsp_types::Range {
                start: Position {
                    line: 10,
                    character: 0,
                },
                end: Position {
                    line: 10,
                    character: 0,
                },
            }),
            range_length: None,
            text: "x".to_string(),
        };
        // Should not panic
        doc.apply_change(change);
    }

    #[test]
    fn test_position_to_offset_empty_document() {
        let mut doc = Document::new(
            "".to_string(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let change = TextDocumentContentChangeEvent {
            range: Some(tower_lsp::lsp_types::Range {
                start: Position {
                    line: 5,
                    character: 3,
                },
                end: Position {
                    line: 5,
                    character: 3,
                },
            }),
            range_length: None,
            text: "x".to_string(),
        };
        doc.apply_change(change);
    }

    #[test]
    fn test_word_at_with_multibyte_characters() {
        // "hello 日本語" - ASCII word, space, then multi-byte chars
        // byte length = 5 + 1 + 9 = 15, char count = 5 + 1 + 3 = 9
        let doc = Document::new(
            "hello 日本語".to_string(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        // col=6 is char index of '日'
        let word = doc.word_at(Position {
            line: 0,
            character: 6,
        });
        assert_eq!(word, Some("日本語".to_string()));

        // col=10 is beyond the last char - should return None, not panic
        // With the byte-length bug (line.len()=15), col=10 < 15 passes guard
        // but chars vec has only 9 elements, so chars[10] would be out of bounds
        let word = doc.word_at(Position {
            line: 0,
            character: 10,
        });
        assert_eq!(word, None);
    }

    #[test]
    fn test_word_at_col_beyond_line_chars() {
        let doc = Document::new(
            "hi".to_string(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        // col=10 is beyond the 2-char line
        let word = doc.word_at(Position {
            line: 0,
            character: 10,
        });
        assert_eq!(word, None);
    }

    #[test]
    fn test_word_at_multibyte_col_between_byte_and_char_len() {
        // "日本" has char_len=2 but byte_len=6
        // col=4 is beyond char_len but within byte_len
        // The bug: line.len() returns 6 (bytes), so col=4 < 6 passes the check
        // but chars vec has only 2 elements, causing index out of bounds
        let doc = Document::new(
            "日本".to_string(),
            Arc::new(ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let word = doc.word_at(Position {
            line: 0,
            character: 4,
        });
        assert_eq!(word, None);
    }
}
