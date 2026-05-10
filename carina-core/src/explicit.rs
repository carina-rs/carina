//! Per-resource user-authored field tree.
//!
//! `ExplicitFields` records which fields the user explicitly wrote in
//! their `.crn` for a resource. The differ projects the actual-state
//! through this tree before computing diffs so server-side default
//! fields the user never authored stop appearing as spurious removals.
//!
//! See `docs/specs/2026-05-10-explicit-fields-design.md`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tree describing which fields the user explicitly wrote in their
/// `.crn` for this resource. Each variant corresponds to a `Value`
/// shape:
///
/// - `Leaf`: the user wrote this position as a scalar value (or as an
///   opaque value with no nested authoring information). Treated as
///   "user wrote the whole thing"; projection keeps the value intact.
/// - `Struct`: the user wrote a struct here. Only the listed
///   `children` are user-authored; struct fields not listed are
///   server-only and are removed by projection.
/// - `List`: the user wrote a list of structs here. `element` is the
///   union of authoring across all elements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ExplicitFields {
    #[default]
    Leaf,
    Struct {
        children: HashMap<String, ExplicitFields>,
    },
    List {
        element: Box<ExplicitFields>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_is_default() {
        let e: ExplicitFields = Default::default();
        assert!(matches!(e, ExplicitFields::Leaf));
    }

    #[test]
    fn struct_round_trips_via_serde() {
        let e = ExplicitFields::Struct {
            children: HashMap::from([
                ("a".into(), ExplicitFields::Leaf),
                (
                    "b".into(),
                    ExplicitFields::Struct {
                        children: HashMap::from([("nested".into(), ExplicitFields::Leaf)]),
                    },
                ),
            ]),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: ExplicitFields = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn list_round_trips_via_serde() {
        let e = ExplicitFields::List {
            element: Box::new(ExplicitFields::Struct {
                children: HashMap::from([("id".into(), ExplicitFields::Leaf)]),
            }),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: ExplicitFields = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn variant_serializes_kebab_case() {
        let leaf_json = serde_json::to_string(&ExplicitFields::Leaf).unwrap();
        assert_eq!(leaf_json, r#"{"kind":"leaf"}"#);
    }
}
