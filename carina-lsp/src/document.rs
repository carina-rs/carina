use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent};

use carina_core::parser::{ParseError, ParsedFile, ProviderContext, parse};

pub struct Document {
    content: Rope,
    parsed: Option<ParsedFile>,
    parse_error: Option<ParseError>,
}

impl Document {
    pub fn new(content: String) -> Self {
        let mut doc = Self {
            content: Rope::from_str(&content),
            parsed: None,
            parse_error: None,
        };
        doc.reparse();
        doc
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
        let text = self.content.to_string();
        let ctx = ProviderContext {
            decryptor: None,
            validators: carina_provider_awscc::schemas::awscc_types::awscc_validators(),
        };
        match parse(&text, &ctx) {
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
        let line_len = self.content.line(line_idx).len_chars();
        let col = (pos.character as usize).min(line_len);
        line_start + col
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    #[allow(dead_code)]
    pub fn parsed(&self) -> Option<&ParsedFile> {
        self.parsed.as_ref()
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

    /// Get the word at the given position
    pub fn word_at(&self, position: Position) -> Option<String> {
        let line = self.line_at(position.line)?;
        let col = position.character as usize;

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

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_to_offset_beyond_last_line() {
        let mut doc = Document::new("hello\nworld".to_string());
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
        let mut doc = Document::new("".to_string());
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
        let doc = Document::new("hello 日本語".to_string());
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
        let doc = Document::new("hi".to_string());
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
        let doc = Document::new("日本".to_string());
        let word = doc.word_at(Position {
            line: 0,
            character: 4,
        });
        assert_eq!(word, None);
    }
}
