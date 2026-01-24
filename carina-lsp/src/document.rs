use ropey::Rope;
use tower_lsp::lsp_types::{Position, TextDocumentContentChangeEvent};

use carina_core::parser::{ParseError, ParsedFile, parse};

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
        match parse(&text) {
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
        let line_start = self.content.line_to_char(pos.line as usize);
        line_start + pos.character as usize
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

        if col > line.len() {
            return None;
        }

        let chars: Vec<char> = line.chars().collect();

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
