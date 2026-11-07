//! Explicit schema-pattern semantics shared by declarations and admitted schemas.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A documented exception to whole-value matching, containing visible text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PatternReason(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyPatternReason;

impl fmt::Display for EmptyPatternReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("substring pattern reason must contain visible text")
    }
}
impl std::error::Error for EmptyPatternReason {}

fn is_invisible_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

impl PatternReason {
    /// Require a visible character while preserving the authored reason verbatim.
    pub fn new(reason: impl Into<String>) -> Result<Self, EmptyPatternReason> {
        let reason = reason.into();
        if !reason
            .chars()
            .any(|c| !c.is_whitespace() && !c.is_control() && !is_invisible_format_char(c))
        {
            Err(EmptyPatternReason)
        } else {
            Ok(Self(reason))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PatternReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Required matching semantics. Substring exceptions must explain why.
///
/// ```compile_fail,E0063
/// use carina_provider_protocol::types::PatternMatch;
/// let _ = PatternMatch::Substring {};
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PatternMatch {
    Full,
    Substring { reason: PatternReason },
}

impl<'de> Deserialize<'de> for PatternMatch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum WireMatch {
            Full {},
            Substring { reason: PatternReason },
        }

        Ok(match WireMatch::deserialize(deserializer)? {
            WireMatch::Full {} => Self::Full,
            WireMatch::Substring { reason } => Self::Substring { reason },
        })
    }
}

/// An authored regex and its mandatory matching semantics.
///
/// ```compile_fail,E0061
/// use carina_provider_protocol::types::PatternSpec;
/// let _ = PatternSpec::new("abc");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternSpec {
    regex: String,
    mode: PatternMatch,
}

/// Exhaustive read-only projection for protocol and fingerprint encoders.
pub struct PatternSpecParts<'a> {
    pub regex: &'a str,
    pub mode: &'a PatternMatch,
}

impl PatternSpec {
    pub fn new(regex: impl Into<String>, mode: PatternMatch) -> Self {
        Self {
            regex: regex.into(),
            mode,
        }
    }

    /// Return the source exactly as authored, without internal boundaries.
    pub fn as_str(&self) -> &str {
        &self.regex
    }
    pub fn mode(&self) -> &PatternMatch {
        &self.mode
    }
    pub fn parts(&self) -> PatternSpecParts<'_> {
        let Self { regex, mode } = self;
        PatternSpecParts { regex, mode }
    }
}

impl AsRef<PatternSpec> for PatternSpec {
    fn as_ref(&self) -> &PatternSpec {
        self
    }
}
