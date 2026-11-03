//! Shared conversions between text offsets and LSP's default UTF-16 code units.

use tower_lsp::lsp_types::Position;

/// Byte and scalar offsets at a UTF-16 column, plus whether the column is exact.
fn column_offsets(line: &str, column: u32) -> (usize, usize, bool) {
    let mut units = 0;
    let mut scalars = 0;
    for (byte, ch) in line.char_indices() {
        if units == column {
            return (byte, scalars, true);
        }
        units += ch.len_utf16() as u32;
        if units > column {
            return (byte, scalars, false);
        }
        scalars += 1;
    }
    (line.len(), scalars, units == column)
}

/// The text before an LSP column. Clamp columns past the line end and never
/// split a surrogate pair if a client supplies a column inside one.
pub fn line_prefix(line: &str, column: u32) -> &str {
    &line[..column_offsets(line, column).0]
}

/// Convert a document position to a UTF-8 byte offset, clamping to the text.
pub fn position_to_byte_offset(text: &str, position: Position) -> usize {
    let mut offset = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == position.line as usize {
            return offset + line_prefix(line, position.character).len();
        }
        offset += line.len() + 1;
    }
    text.len()
}

/// Convert a document byte offset to a UTF-16 position. Invalid UTF-8
/// boundaries and offsets outside the source cannot be rendered.
pub fn byte_offset_to_position(text: &str, offset: usize) -> Option<Position> {
    let prefix = text.get(..offset)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let tail = prefix.rsplit('\n').next().unwrap_or("");
    Some(Position::new(line, char_len(tail)))
}

/// Convert a byte offset within a string to a UTF-16 code-unit offset.
///
/// # Panics
/// Panics if `byte_offset` is not on a character boundary or exceeds the string length.
pub fn byte_offset_to_char_offset(s: &str, byte_offset: usize) -> u32 {
    s[..byte_offset].encode_utf16().count() as u32
}

/// Return the UTF-16 code-unit count of a string.
pub fn char_len(s: &str) -> u32 {
    s.encode_utf16().count() as u32
}

/// Convert an internal Unicode-scalar index to an LSP column.
pub fn char_offset_to_utf16_offset(s: &str, offset: usize) -> u32 {
    offset as u32
        + s.chars()
            .take(offset)
            .map(|ch| ch.len_utf16() as u32 - 1)
            .sum::<u32>()
}

/// Convert an LSP column to a Unicode-scalar index without splitting a surrogate pair.
pub fn utf16_offset_to_char_offset(s: &str, offset: u32) -> Option<usize> {
    let (_, scalars, exact) = column_offsets(s, offset);
    exact.then_some(scalars)
}

/// Calculate the character offset of leading whitespace in a line.
///
/// Measure the leading whitespace using the same UTF-16 units as other columns.
pub fn leading_whitespace_chars(line: &str) -> u32 {
    let trimmed = line.trim_start();
    byte_offset_to_char_offset(line, line.len() - trimmed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byte_offset_to_char_offset_ascii() {
        assert_eq!(byte_offset_to_char_offset("hello", 0), 0);
        assert_eq!(byte_offset_to_char_offset("hello", 3), 3);
        assert_eq!(byte_offset_to_char_offset("hello", 5), 5);
    }

    #[test]
    fn test_byte_offset_to_char_offset_multibyte() {
        // "あいう" - each character is 3 bytes in UTF-8
        let s = "あいう";
        assert_eq!(byte_offset_to_char_offset(s, 0), 0); // before あ
        assert_eq!(byte_offset_to_char_offset(s, 3), 1); // before い
        assert_eq!(byte_offset_to_char_offset(s, 6), 2); // before う
        assert_eq!(byte_offset_to_char_offset(s, 9), 3); // end
    }

    #[test]
    fn test_byte_offset_to_char_offset_mixed() {
        // "ab日本cd" - a(1) b(1) 日(3) 本(3) c(1) d(1) = 10 bytes, 6 chars
        let s = "ab日本cd";
        assert_eq!(byte_offset_to_char_offset(s, 0), 0); // a
        assert_eq!(byte_offset_to_char_offset(s, 2), 2); // 日
        assert_eq!(byte_offset_to_char_offset(s, 5), 3); // 本
        assert_eq!(byte_offset_to_char_offset(s, 8), 4); // c
        assert_eq!(byte_offset_to_char_offset(s, 9), 5); // d
    }

    #[test]
    fn test_char_len_ascii() {
        assert_eq!(char_len("hello"), 5);
        assert_eq!(char_len(""), 0);
    }

    #[test]
    fn test_char_len_multibyte() {
        assert_eq!(char_len("あいう"), 3);
        assert_eq!(char_len("ab日本cd"), 6);
    }

    #[test]
    fn test_leading_whitespace_chars() {
        assert_eq!(leading_whitespace_chars("    hello"), 4);
        assert_eq!(leading_whitespace_chars("hello"), 0);
        assert_eq!(leading_whitespace_chars("\thello"), 1);
        assert_eq!(leading_whitespace_chars("  \tname = \"あ\""), 3);
    }
}
