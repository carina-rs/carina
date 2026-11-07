//! Host rendering for the shared protocol pattern metadata.

use std::fmt;

use crate::provider_text::{DiagnosticText, DiagnosticTextBuilder};

use super::{PatternMatch, PatternSpec};

/// A pattern's safe diagnostic spelling. Protocol types carry data only.
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
        Self {
            spec,
            source: SourceDisplay::Slashes,
        }
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
                text.push_str(&format!("{:?} (", self.spec.as_str()));
            }
            SourceDisplay::Omitted => {}
        }
        match self.spec.mode() {
            PatternMatch::Full => text.push_str("Full"),
            PatternMatch::Substring { reason } => {
                text.push_str("Substring: ");
                text.push_inline_str(reason.as_str());
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
    fn pattern_reason_display_escapes_controls_without_changing_wire_text() {
        let reason = "prefix-only\n\u{1b}[31m metadata";
        let mode = PatternMatch::Substring {
            reason: PatternReason::new(reason).unwrap(),
        };
        assert_eq!(
            PatternSpecDisplay::new(&PatternSpec::new("abc", mode.clone()))
                .mode_only()
                .to_string(),
            "Substring: prefix-only\\n\\u{1b}[31m metadata"
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
