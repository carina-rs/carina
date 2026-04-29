//! Byte-slice helpers shared by the LSP integration tests.

/// Find the first occurrence of `needle` within `haystack`, returning its
/// starting index. Used by the test client to locate the `\r\n\r\n` boundary
/// between LSP message header and body.
#[allow(dead_code)]
pub fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
