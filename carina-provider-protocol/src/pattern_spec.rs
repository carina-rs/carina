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

// Complete Default_Ignorable_Code_Point property from Unicode 16.0.0:
// https://www.unicode.org/Public/16.0.0/ucd/DerivedCoreProperties.txt
// Includes the Hangul fillers U+115F, U+1160, U+3164 and U+FFA0.
const DEFAULT_IGNORABLE_CODE_POINT_RANGES: &[std::ops::RangeInclusive<char>] = &[
    '\u{ad}'..='\u{ad}',
    '\u{34f}'..='\u{34f}',
    '\u{61c}'..='\u{61c}',
    '\u{115f}'..='\u{1160}',
    '\u{17b4}'..='\u{17b5}',
    '\u{180b}'..='\u{180d}',
    '\u{180e}'..='\u{180e}',
    '\u{180f}'..='\u{180f}',
    '\u{200b}'..='\u{200f}',
    '\u{202a}'..='\u{202e}',
    '\u{2060}'..='\u{2064}',
    '\u{2065}'..='\u{2065}',
    '\u{2066}'..='\u{206f}',
    '\u{3164}'..='\u{3164}',
    '\u{fe00}'..='\u{fe0f}',
    '\u{feff}'..='\u{feff}',
    '\u{ffa0}'..='\u{ffa0}',
    '\u{fff0}'..='\u{fff8}',
    '\u{1bca0}'..='\u{1bca3}',
    '\u{1d173}'..='\u{1d17a}',
    '\u{e0000}'..='\u{e0000}',
    '\u{e0001}'..='\u{e0001}',
    '\u{e0002}'..='\u{e001f}',
    '\u{e0020}'..='\u{e007f}',
    '\u{e0080}'..='\u{e00ff}',
    '\u{e0100}'..='\u{e01ef}',
    '\u{e01f0}'..='\u{e0fff}',
];

/// Whether a character has Unicode 16.0.0's Default_Ignorable_Code_Point property.
pub fn is_default_ignorable(c: char) -> bool {
    DEFAULT_IGNORABLE_CODE_POINT_RANGES
        .iter()
        .any(|range| range.contains(&c))
}

impl PatternReason {
    /// Require a visible character while preserving the authored reason verbatim.
    pub fn new(reason: impl Into<String>) -> Result<Self, EmptyPatternReason> {
        let reason = reason.into();
        if !reason
            .chars()
            .any(|c| !c.is_whitespace() && !c.is_control() && !is_default_ignorable(c))
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
