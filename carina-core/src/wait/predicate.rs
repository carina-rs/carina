//! Typed predicate AST for the `wait` construct.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The MVP supports `<target>.<attr> == <value>` only; the enum
//! is shaped to grow (`NotEquals`, `And`, `Or`, comparisons, `In`)
//! without breaking call sites.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::resource::{ConcreteValue, Value};

/// Dotted path into a target's attribute tree.
///
/// Resolution walks the path one segment at a time, descending into
/// nested `ConcreteValue::Map` values. For example, an `AttrPath`
/// with `segments = ["renewal_summary", "renewal_status"]` resolves
/// to `attrs["renewal_summary"]["renewal_status"]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttrPath {
    pub segments: Vec<String>,
}

impl AttrPath {
    pub fn single(name: impl Into<String>) -> Self {
        Self {
            segments: vec![name.into()],
        }
    }

    /// Walk this path into `attrs`, descending through nested
    /// `ConcreteValue::Map` values as needed. Returns the leaf `Value`
    /// or `None` if any segment is missing or not a map at a non-leaf.
    pub(crate) fn resolve<'a>(&self, attrs: &'a HashMap<String, Value>) -> Option<&'a Value> {
        let (first, rest) = self.segments.split_first()?;
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
    fn resolve_returns_none_for_empty_path() {
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("pending".to_string())),
        )]);
        let path = AttrPath {
            segments: Vec::new(),
        };

        assert!(path.resolve(&attrs).is_none());
    }
}
