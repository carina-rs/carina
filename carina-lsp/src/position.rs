//! Utility functions for converting between byte offsets and character offsets.
//!
//! LSP protocol positions use UTF-16 code units by default. For characters in the Basic
//! Multilingual Plane (including ASCII, Japanese, most common non-ASCII), UTF-16 code unit
//! count equals Unicode codepoint count. These functions use codepoint counting
//! (`chars().count()`), which is correct for all BMP characters and sufficient for Carina
//! DSL content.

/// Convert a byte offset within a string to a character (codepoint) offset.
///
/// # Panics
/// Panics if `byte_offset` is not on a character boundary or exceeds the string length.
pub fn byte_offset_to_char_offset(s: &str, byte_offset: usize) -> u32 {
    s[..byte_offset].chars().count() as u32
}

/// Return the character count (not byte length) of a string.
pub fn char_len(s: &str) -> u32 {
    s.chars().count() as u32
}

/// Calculate the character offset of leading whitespace in a line.
///
/// Since leading whitespace consists only of ASCII spaces and tabs,
/// the byte length equals the character count. This function uses
/// byte arithmetic but is safe because whitespace is always ASCII.
pub fn leading_whitespace_chars(line: &str) -> u32 {
    let trimmed = line.trim_start();
    // Leading whitespace is ASCII (spaces/tabs), so byte count == char count
    (line.len() - trimmed.len()) as u32
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
