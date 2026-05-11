//! Typed predicate AST for the `wait` construct.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The MVP supports `<target>.<attr> == <value>` only; the enum
//! is shaped to grow (`NotEquals`, `And`, `Or`, comparisons, `In`)
//! without breaking call sites.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::resource::Value;

/// Dotted path into a target's attribute tree.
///
/// MVP only resolves a single top-level segment; nested-field traversal
/// (`renewal_summary.renewal_status`) is reserved for a follow-up that
/// teaches the evaluator to descend into `Value::Concrete(ConcreteValue::Map)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttrPath {
    pub segments: Vec<String>,
}

impl AttrPath {
    pub fn single(name: impl Into<String>) -> Self {
        Self {
            segments: vec![name.into()],
        }
    }
}

/// Typed predicate AST evaluated against a target's attribute snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitPredicate {
    Equals { attr: AttrPath, value: Value },
}

impl WaitPredicate {
    /// Evaluate against the target's `read()` attribute map. Returns
    /// `true` when the predicate is satisfied; missing attributes and
    /// type mismatches both yield `false` (the wait keeps polling).
    pub fn evaluate(&self, attrs: &HashMap<String, Value>) -> bool {
        match self {
            WaitPredicate::Equals { attr, value } => {
                resolve(attrs, &attr.segments).is_some_and(|v| v == value)
            }
        }
    }
}

fn resolve<'a>(attrs: &'a HashMap<String, Value>, path: &[String]) -> Option<&'a Value> {
    let first = attrs.get(path.first()?)?;
    if path.len() == 1 {
        return Some(first);
    }
    None
}
