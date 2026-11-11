//! Host rendering for the shared protocol pattern metadata.

use std::fmt;

use crate::provider_text::{DiagnosticQuote, DiagnosticText, DiagnosticTextBuilder};

use super::{PatternMatch, PatternSpec};

/// A pattern's safe diagnostic spelling. Ordinary sources stay verbatim between
/// slashes; sources needing inline escapes or containing `/ (` use quoted
/// strings. Reasons needing escapes or containing parentheses or quotes are
/// quoted with the same helper, keeping controls and metadata unambiguous.
/// Protocol types carry data only.
#[derive(Clone, Copy)]
pub struct PatternSpecDisplay<'a> {
    spec: &'a PatternSpec,
    source: SourceDisplay,
}

#[derive(Clone, Copy)]
enum SourceDisplay {
    Slashes,
    Quoted,
    Omitted,
}

impl<'a> PatternSpecDisplay<'a> {
    pub fn new(spec: &'a PatternSpec) -> Self {
        let source = if spec.as_str().contains("/ (")
            || spec
                .as_str()
                .chars()
                .any(|ch| !matches!(ch, '"' | '\\') && DiagnosticQuote::Double.escapes(ch))
        {
            SourceDisplay::Quoted
        } else {
            SourceDisplay::Slashes
        };
        Self { spec, source }
    }

    pub(crate) fn quoted(mut self) -> Self {
        self.source = SourceDisplay::Quoted;
        self
    }

    pub fn mode_only(mut self) -> Self {
        self.source = SourceDisplay::Omitted;
        self
    }

    pub(crate) fn push_to(self, text: &mut DiagnosticTextBuilder) {
        match self.source {
            SourceDisplay::Slashes => {
                text.push_str("/");
                text.push_str(self.spec.as_str());
                text.push_str("/ (");
            }
            SourceDisplay::Quoted => {
                text.push_quoted_str(self.spec.as_str(), DiagnosticQuote::Double);
                text.push_str(" (");
            }
            SourceDisplay::Omitted => {}
        }
        match self.spec.mode() {
            PatternMatch::Full => text.push_str("Full"),
            PatternMatch::Substring { reason } => {
                text.push_str("Substring: ");
                let reason = reason.as_str();
                if reason
                    .chars()
                    .any(|ch| matches!(ch, '(' | ')') || DiagnosticQuote::Double.escapes(ch))
                {
                    text.push_quoted_str(reason, DiagnosticQuote::Double);
                } else {
                    text.push_inline_str(reason);
                }
            }
        }
        if !matches!(self.source, SourceDisplay::Omitted) {
            text.push_str(")");
        }
    }

    fn text(self) -> DiagnosticText {
        let mut text = DiagnosticTextBuilder::default();
        self.push_to(&mut text);
        text.finish()
    }
}

impl fmt::Display for PatternSpecDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.text(), f)
    }
}

/// An error's complete pattern, with host rendering and raw metadata access.
#[derive(Clone, PartialEq, Eq)]
pub struct DiagnosticPatternSpec(Box<PatternSpec>);

impl From<PatternSpec> for DiagnosticPatternSpec {
    fn from(spec: PatternSpec) -> Self {
        Self(Box::new(spec))
    }
}

impl AsRef<PatternSpec> for DiagnosticPatternSpec {
    fn as_ref(&self) -> &PatternSpec {
        &self.0
    }
}

impl DiagnosticPatternSpec {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for DiagnosticPatternSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for DiagnosticPatternSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        PatternSpecDisplay::new(self.as_ref()).fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::PatternReason;

    #[test]
    fn review_4327_pattern_metadata_distinguishes_source_and_mode_delimiters() {
        let full = PatternSpec::new("x/ (Substring: r)", PatternMatch::Full);
        let substring = PatternSpec::new(
            "x",
            PatternMatch::Substring {
                reason: PatternReason::new("r)/ (Full").unwrap(),
            },
        );
        assert_ne!(
            PatternSpecDisplay::new(&full).to_string(),
            PatternSpecDisplay::new(&substring).to_string()
        );
        for (spec, expected) in [
            (&full, r#""x/ (Substring: r)" (Full)"#),
            (&substring, r#"/x/ (Substring: "r)/ (Full")"#),
        ] {
            let wire = serde_json::to_value(spec).unwrap();
            assert_eq!(PatternSpecDisplay::new(spec).to_string(), expected);
            assert_eq!(
                DiagnosticPatternSpec::from(spec.clone()).to_string(),
                expected
            );
            assert_eq!(
                super::super::CompiledPattern::try_new(spec)
                    .unwrap()
                    .to_string(),
                expected
            );
            assert_eq!(serde_json::to_value(spec).unwrap(), wire);
            assert_eq!(serde_json::from_value::<PatternSpec>(wire).unwrap(), *spec);
        }
        assert_eq!(
            PatternSpecDisplay::new(&substring).mode_only().to_string(),
            r#"Substring: "r)/ (Full""#
        );
        assert_eq!(
            PatternSpecDisplay::new(&substring).quoted().to_string(),
            r#""x" (Substring: "r)/ (Full")"#
        );
    }

    #[test]
    fn review_4327_pattern_metadata_distinguishes_literal_and_escaped_reason_lf() {
        let literal = PatternSpec::new(
            "x",
            PatternMatch::Substring {
                reason: PatternReason::new("why\n").unwrap(),
            },
        );
        let escaped = PatternSpec::new(
            "x",
            PatternMatch::Substring {
                reason: PatternReason::new(r"why\n").unwrap(),
            },
        );
        assert_ne!(
            PatternSpecDisplay::new(&literal).to_string(),
            PatternSpecDisplay::new(&escaped).to_string()
        );
        for (spec, reason, expected) in [
            (&literal, "why\n", r#""why\n""#),
            (&escaped, r"why\n", r#""why\\n""#),
        ] {
            assert_eq!(
                PatternSpecDisplay::new(spec).to_string(),
                format!("/x/ (Substring: {expected})")
            );
            assert_eq!(
                PatternSpecDisplay::new(spec).mode_only().to_string(),
                format!("Substring: {expected}")
            );
            assert_eq!(
                PatternSpecDisplay::new(spec).quoted().to_string(),
                format!("\"x\" (Substring: {expected})")
            );
            assert_eq!(
                serde_json::to_value(spec.mode()).unwrap(),
                serde_json::json!({"kind": "substring", "reason": reason})
            );
        }
    }

    #[test]
    fn review_4327_pattern_metadata_quotes_reserved_tokens_and_keeps_simple_forms() {
        for source in ["x/(", "x/  (", "x (r)", r"[a-z]\d+", r#"["'\\]+"#] {
            assert_eq!(
                PatternSpecDisplay::new(&PatternSpec::new(source, PatternMatch::Full)).to_string(),
                format!("/{source}/ (Full)")
            );
        }
        for (source, expected) in [
            ("x/ (r)", r#""x/ (r)" (Full)"#),
            ("x/ (", r#""x/ (" (Full)"#),
            ("x/ (r)\"\\q", r#""x/ (r)\"\\q" (Full)"#),
        ] {
            let spec = PatternSpec::new(source, PatternMatch::Full);
            assert_eq!(PatternSpecDisplay::new(&spec).to_string(), expected);
            assert_eq!(spec.as_str(), source);
        }
        for (reason, expected) in [
            ("a reason", "a reason"),
            ("why's fine", "why's fine"),
            ("é", "é"),
            ("why(", r#""why(""#),
            ("why)", r#""why)""#),
            ("why\"quote", r#""why\"quote""#),
            (r"why\word", r#""why\\word""#),
            ("why\t", r#""why\t""#),
            ("why\u{202e}", r#""why\u{202e}""#),
            ("why\u{2028}", r#""why\u{2028}""#),
        ] {
            let mode = PatternMatch::Substring {
                reason: PatternReason::new(reason).unwrap(),
            };
            let spec = PatternSpec::new("x", mode.clone());
            assert_eq!(
                PatternSpecDisplay::new(&spec).to_string(),
                format!("/x/ (Substring: {expected})")
            );
            assert_eq!(
                PatternSpecDisplay::new(&spec).mode_only().to_string(),
                format!("Substring: {expected}")
            );
            assert_eq!(
                serde_json::to_value(&mode).unwrap(),
                serde_json::json!({"kind": "substring", "reason": reason})
            );
        }
    }

    #[test]
    fn review_4327_pattern_display_distinguishes_literal_and_escaped_lf() {
        let literal_lf = PatternSpec::new("(?x)a#comment\nb", PatternMatch::Full);
        let escaped_lf = PatternSpec::new(r"(?x)a#comment\nb", PatternMatch::Full);
        let literal_display = PatternSpecDisplay::new(&literal_lf).to_string();
        let escaped_display = PatternSpecDisplay::new(&escaped_lf).to_string();
        assert_ne!(literal_display, escaped_display);
        assert_eq!(literal_display, r#""(?x)a#comment\nb" (Full)"#);
        assert_eq!(escaped_display, r"/(?x)a#comment\nb/ (Full)");
        assert_eq!(
            PatternSpecDisplay::new(&escaped_lf).quoted().to_string(),
            r#""(?x)a#comment\\nb" (Full)"#
        );

        let literal = super::super::CompiledPattern::try_new(&literal_lf).unwrap();
        let escaped = super::super::CompiledPattern::try_new(&escaped_lf).unwrap();
        assert!(literal.is_match("ab"));
        assert!(!escaped.is_match("ab"));
        assert!(!literal.is_match("a"));
        assert!(escaped.is_match("a"));
        assert_eq!(literal.as_str(), "(?x)a#comment\nb");
        assert_eq!(escaped.as_str(), r"(?x)a#comment\nb");
    }

    #[test]
    fn review_4327_pattern_display_quotes_bidi_and_preserves_safe_sources() {
        let em_space = '\u{2003}'.escape_debug().collect::<String>();
        let em_space_display = if em_space == "\u{2003}" {
            format!("/a{em_space}b/ (Full)")
        } else {
            format!("\"a{em_space}b\" (Full)")
        };
        for source in [r"^[a-z]\d+$", r"a\u{202e}\b", r#"["'\\]+"#, "é"] {
            let spec = PatternSpec::new(source, PatternMatch::Full);
            assert_eq!(
                PatternSpecDisplay::new(&spec).to_string(),
                format!("/{source}/ (Full)")
            );
        }
        for (source, expected) in [
            ("a\u{202e}\\b", r#""a\u{202e}\\b" (Full)"#),
            ("a\"b\\c\n", r#""a\"b\\c\n" (Full)"#),
            ("a\u{2028}b", r#""a\u{2028}b" (Full)"#),
            ("a\u{2003}b", em_space_display.as_str()),
            ("a\u{fe0f}b", r#""a\u{fe0f}b" (Full)"#),
        ] {
            let spec = PatternSpec::new(source, PatternMatch::Full);
            assert_eq!(PatternSpecDisplay::new(&spec).to_string(), expected);
            assert_eq!(spec.as_str(), source);
        }
    }

    #[test]
    fn review_4327_pattern_source_and_reason_are_inline_in_every_display() {
        let source = "a\u{202e}\nb";
        for (mode, mode_text) in [
            (PatternMatch::Full, "Full"),
            (
                PatternMatch::Substring {
                    reason: PatternReason::new("why\u{202e}\n\t").unwrap(),
                },
                "Substring: \"why\\u{202e}\\n\\t\"",
            ),
        ] {
            let spec = PatternSpec::new(source, mode);
            assert_eq!(
                PatternSpecDisplay::new(&spec).to_string(),
                format!("\"a\\u{{202e}}\\nb\" ({mode_text})")
            );
            assert_eq!(
                PatternSpecDisplay::new(&spec).quoted().to_string(),
                format!("\"a\\u{{202e}}\\nb\" ({mode_text})")
            );
            assert_eq!(
                PatternSpecDisplay::new(&spec).mode_only().to_string(),
                mode_text
            );
            assert_eq!(spec.as_str(), source, "wire metadata must remain raw");
        }
    }

    #[test]
    fn pattern_reason_display_escapes_controls_without_changing_wire_text() {
        let reason = "prefix-only\n\u{1b}[31m metadata";
        let mode = PatternMatch::Substring {
            reason: PatternReason::new(reason).unwrap(),
        };
        assert_eq!(
            PatternSpecDisplay::new(&PatternSpec::new("abc", mode.clone()))
                .mode_only()
                .to_string(),
            "Substring: \"prefix-only\\n\\u{1b}[31m metadata\""
        );
        let json = serde_json::json!({"kind": "substring", "reason": reason});
        assert_eq!(serde_json::to_value(&mode).unwrap(), json);
        assert_eq!(serde_json::from_value::<PatternMatch>(json).unwrap(), mode);
        assert_eq!(
            serde_json::to_value(PatternMatch::Full).unwrap(),
            serde_json::json!({"kind": "full"})
        );
    }
}
