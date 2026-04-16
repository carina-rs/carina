//! Partial-line parsing of `let <name> = <rhs>` bindings.
//!
//! The LSP repeatedly scans lines the user is still typing, so it can't rely on
//! the pest parser (which rejects incomplete input). This helper is the one
//! place that knows how to peel a `let` header off such a line.

/// Parse a `let <name> = <rhs>` header from a single line.
///
/// Returns `(binding_name, rhs)` on success, where `rhs` is the trimmed text
/// after the `=`. The binding name must be a non-empty ASCII identifier
/// (alphanumeric or `_`); otherwise `None` is returned. The ASCII restriction
/// deliberately matches the pest grammar (`identifier = ASCII_ALPHA ~ ...`) so
/// the LSP and parser agree on what counts as a valid binding.
///
/// Leading whitespace on `line` is ignored, and so is whitespace around the
/// name and around the `=`. Trailing whitespace on `rhs` is preserved so the
/// caller can decide whether to trim further.
pub(crate) fn parse_let_header(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix("let ")?;
    let eq_pos = rest.find('=')?;
    let name = rest[..eq_pos].trim();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let rhs = rest[eq_pos + 1..].trim_start();
    Some((name, rhs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_let_binding() {
        assert_eq!(parse_let_header("let foo = bar"), Some(("foo", "bar")));
    }

    #[test]
    fn trims_leading_whitespace() {
        assert_eq!(parse_let_header("    let foo = bar"), Some(("foo", "bar")));
        assert_eq!(parse_let_header("\tlet foo = bar"), Some(("foo", "bar")));
    }

    #[test]
    fn accepts_identifier_with_digits_and_underscore() {
        assert_eq!(
            parse_let_header("let my_var_1 = expr"),
            Some(("my_var_1", "expr"))
        );
    }

    #[test]
    fn accepts_discard_pattern() {
        // `_` is a valid binding name at the parse level; callers that want to
        // exclude the discard pattern must check it themselves.
        assert_eq!(parse_let_header("let _ = expr"), Some(("_", "expr")));
    }

    #[test]
    fn rhs_is_trimmed_at_start_only() {
        let (name, rhs) = parse_let_header("let x =  \t  value   ").unwrap();
        assert_eq!(name, "x");
        assert_eq!(rhs, "value   ");
    }

    #[test]
    fn tolerates_no_spaces_around_equals() {
        assert_eq!(parse_let_header("let x=expr"), Some(("x", "expr")));
    }

    #[test]
    fn rejects_without_let_keyword() {
        assert_eq!(parse_let_header("x = expr"), None);
        assert_eq!(parse_let_header("letx = expr"), None);
        assert_eq!(parse_let_header("letter = expr"), None);
    }

    #[test]
    fn rejects_without_equals_sign() {
        assert_eq!(parse_let_header("let foo"), None);
    }

    #[test]
    fn rejects_empty_binding_name() {
        assert_eq!(parse_let_header("let  = expr"), None);
        assert_eq!(parse_let_header("let = expr"), None);
    }

    #[test]
    fn rejects_invalid_identifier_characters() {
        assert_eq!(parse_let_header("let foo-bar = expr"), None);
        assert_eq!(parse_let_header("let foo.bar = expr"), None);
        assert_eq!(parse_let_header("let foo bar = expr"), None);
    }

    #[test]
    fn allows_empty_rhs() {
        // Common when user has typed `let name = ` and is waiting for completion.
        assert_eq!(parse_let_header("let x = "), Some(("x", "")));
        assert_eq!(parse_let_header("let x ="), Some(("x", "")));
    }
}
