//! Typed predicate AST for the `wait` construct.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The MVP supports `<target>.<attr> == <value>` only; the enum
//! is shaped to grow (`NotEquals`, `And`, `Or`, comparisons, `In`)
//! without breaking call sites.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::resource::{ConcreteValue, Value};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrPathError {
    #[error("AttrPath segments cannot be empty")]
    Empty,
}

/// Dotted path into a target's attribute tree.
///
/// Resolution walks the path one segment at a time, descending into
/// nested `ConcreteValue::Map` values. For example, an `AttrPath`
/// with `segments = ["renewal_summary", "renewal_status"]` resolves
/// to `attrs["renewal_summary"]["renewal_status"]`.
///
/// The `segments` field is private to make the empty-path state
/// unrepresentable: construct via `AttrPath::single` for a single
/// top-level attribute or `AttrPath::try_new` for a multi-segment
/// path. Serde deserialization also rejects empty segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "AttrPathSerde", try_from = "AttrPathSerde")]
pub struct AttrPath {
    segments: Vec<String>,
}

impl AttrPath {
    pub fn single(name: impl Into<String>) -> Self {
        Self {
            segments: vec![name.into()],
        }
    }

    pub fn try_new(segments: Vec<String>) -> Result<Self, AttrPathError> {
        if segments.is_empty() {
            return Err(AttrPathError::Empty);
        }
        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Walk this path into `attrs`, descending through nested
    /// `ConcreteValue::Map` values as needed. Returns the leaf `Value`
    /// or `None` if any segment is missing or not a map at a non-leaf.
    pub(crate) fn resolve<'a>(&self, attrs: &'a HashMap<String, Value>) -> Option<&'a Value> {
        let (first, rest) = self.segments().split_first()?;
        let mut current = attrs.get(first)?;
        for segment in rest {
            match current {
                Value::Concrete(ConcreteValue::Map(map)) => {
                    current = map.get(segment)?;
                }
                _ => return None,
            }
        }
        Some(current)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttrPathSerde {
    segments: Vec<String>,
}

impl From<AttrPath> for AttrPathSerde {
    fn from(p: AttrPath) -> Self {
        Self {
            segments: p.segments,
        }
    }
}

impl TryFrom<AttrPathSerde> for AttrPath {
    type Error = AttrPathError;

    fn try_from(p: AttrPathSerde) -> Result<Self, Self::Error> {
        AttrPath::try_new(p.segments)
    }
}

/// Typed predicate AST evaluated against a target's attribute snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitPredicate {
    Equals { attr: AttrPath, value: Value },
}

impl WaitPredicate {
    /// Attribute paths referenced by this predicate, in evaluation
    /// order. Heartbeat consumers use this to show the operator the
    /// value the wait is actually polling. Future predicate variants
    /// (And, Or, NotEquals, comparisons) extend this list.
    pub fn watched_attrs(&self) -> Vec<&AttrPath> {
        match self {
            WaitPredicate::Equals { attr, .. } => vec![attr],
        }
    }

    /// Evaluate against the target's `read()` attribute map. Returns
    /// `true` when the predicate is satisfied; missing attributes and
    /// type mismatches both yield `false` (the wait keeps polling).
    pub fn evaluate(&self, attrs: &HashMap<String, Value>) -> bool {
        match self {
            WaitPredicate::Equals { attr, value } => {
                attr.resolve(attrs).is_some_and(|v| v == value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ConcreteValue;
    use std::collections::HashMap;

    #[test]
    fn equals_watched_attrs_returns_the_predicate_attr() {
        let attr = AttrPath::single("status");
        let predicate = WaitPredicate::Equals {
            attr: attr.clone(),
            value: Value::Concrete(ConcreteValue::String("ready".to_string())),
        };

        assert_eq!(predicate.watched_attrs(), vec![&attr]);
    }

    #[test]
    fn attr_path_try_new_rejects_empty_segments() {
        assert!(matches!(
            AttrPath::try_new(vec![]),
            Err(AttrPathError::Empty)
        ));
    }

    #[test]
    fn attr_path_try_new_accepts_non_empty_segments() {
        let path =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();

        assert_eq!(path.segments(), &["renewal_summary", "renewal_status"]);
    }

    #[test]
    fn attr_path_segments_field_is_not_publicly_accessible() {
        let path = AttrPath::single("status");
        let segments: &[String] = path.segments();

        assert_eq!(segments, &["status"]);
    }

    #[test]
    fn attr_path_serde_roundtrip_preserves_wire_format() {
        let path =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, r#"{"segments":["renewal_summary","renewal_status"]}"#);
        let parsed: AttrPath = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segments(), &["renewal_summary", "renewal_status"]);
    }

    #[test]
    fn attr_path_serde_rejects_empty_segments() {
        let json = r#"{"segments":[]}"#;
        let result: Result<AttrPath, _> = serde_json::from_str(json);

        assert!(result.is_err());
    }

    #[test]
    fn resolve_returns_none_for_missing_attr() {
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("pending".to_string())),
        )]);
        let path = AttrPath::single("missing");

        assert!(path.resolve(&attrs).is_none());
    }
}
