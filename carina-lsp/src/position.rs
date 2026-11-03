//! Conversion from UTF-8 byte offsets to LSP's default UTF-16 code units.

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
    let mut units = 0;
    let mut count = 0;
    for ch in s.chars() {
        if units == offset {
            return Some(count);
        }
        units += ch.len_utf16() as u32;
        count += 1;
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(count)
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
