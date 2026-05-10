//! Per-resource user-authored field tree.
//!
//! `ExplicitFields` records which fields the user explicitly wrote in
//! their `.crn` for a resource. The differ projects the actual-state
//! through this tree before computing diffs so server-side default
//! fields the user never authored stop appearing as spurious removals.
//!
//! See `docs/specs/2026-05-10-explicit-fields-design.md`.

use crate::resource::{Resource, Value};
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

/// Build an `ExplicitFields::Struct` rooted at a resource's top-level
/// attributes. Underscore-prefixed keys (internal attributes) are
/// excluded.
pub fn build_from_resource(resource: &Resource) -> ExplicitFields {
    ExplicitFields::Struct {
        children: resource
            .attributes
            .iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), build_from_value(v)))
            .collect(),
    }
}

/// Build an `ExplicitFields` tree describing the structural shape of a
/// `Value`. `Value::Map` is treated as a struct (each key becomes a
/// struct child); `Value::List` becomes a `List` whose element is the
/// union of authoring across all elements; everything else is a `Leaf`.
pub fn build_from_value(value: &Value) -> ExplicitFields {
    match value {
        Value::Map(fields) => ExplicitFields::Struct {
            children: fields
                .iter()
                .map(|(k, v)| (k.clone(), build_from_value(v)))
                .collect(),
        },
        Value::List(items) => ExplicitFields::List {
            element: Box::new(
                items
                    .iter()
                    .map(build_from_value)
                    .fold(ExplicitFields::Leaf, merge),
            ),
        },
        _ => ExplicitFields::Leaf,
    }
}

/// Combine two `ExplicitFields` trees by union semantics. Used to fold
/// the per-element trees of a list-of-structs into a single common
/// element shape.
pub fn merge(a: ExplicitFields, b: ExplicitFields) -> ExplicitFields {
    use ExplicitFields::*;
    match (a, b) {
        (Leaf, b) => b,
        (a, Leaf) => a,
        (
            Struct {
                children: mut a_children,
            },
            Struct {
                children: b_children,
            },
        ) => {
            for (k, v) in b_children {
                let merged = match a_children.remove(&k) {
                    Some(existing) => merge(existing, v),
                    None => v,
                };
                a_children.insert(k, merged);
            }
            Struct {
                children: a_children,
            }
        }
        (List { element: a }, List { element: b }) => List {
            element: Box::new(merge(*a, *b)),
        },
        // Mismatched shapes shouldn't occur for well-typed inputs;
        // prefer the structural variant on the left.
        (a, _) => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

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

    #[test]
    fn build_from_value_scalar_is_leaf() {
        let v = Value::String("x".into());
        assert_eq!(build_from_value(&v), ExplicitFields::Leaf);
    }

    #[test]
    fn build_from_value_struct_collects_children() {
        let mut fields = IndexMap::new();
        fields.insert("a".into(), Value::String("x".into()));
        fields.insert("b".into(), Value::Int(1));
        let v = Value::Map(fields);
        let ExplicitFields::Struct { children } = build_from_value(&v) else {
            panic!("expected Struct");
        };
        assert_eq!(children.len(), 2);
        assert!(matches!(children["a"], ExplicitFields::Leaf));
        assert!(matches!(children["b"], ExplicitFields::Leaf));
    }

    #[test]
    fn build_from_value_list_unions_element_authoring() {
        let mut e1 = IndexMap::new();
        e1.insert("a".into(), Value::Int(1));
        e1.insert("b".into(), Value::Int(1));
        let mut e2 = IndexMap::new();
        e2.insert("b".into(), Value::Int(2));
        e2.insert("c".into(), Value::Int(2));
        let v = Value::List(vec![Value::Map(e1), Value::Map(e2)]);
        let ExplicitFields::List { element } = build_from_value(&v) else {
            panic!("expected List");
        };
        let ExplicitFields::Struct { children } = *element else {
            panic!("expected Struct element");
        };
        let mut keys: Vec<&str> = children.keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn build_from_resource_skips_underscore_attrs() {
        let mut r = Resource::with_provider("aws", "s3.Bucket", "x");
        r.set_attr("name".to_string(), Value::String("hi".into()));
        r.set_attr("_internal".to_string(), Value::String("skip".into()));
        let ExplicitFields::Struct { children } = build_from_resource(&r) else {
            panic!("expected Struct at root");
        };
        assert!(children.contains_key("name"));
        assert!(!children.contains_key("_internal"));
    }

    #[test]
    fn merge_struct_into_struct_unions_keys() {
        let a = ExplicitFields::Struct {
            children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
        };
        let b = ExplicitFields::Struct {
            children: HashMap::from([("b".into(), ExplicitFields::Leaf)]),
        };
        let ExplicitFields::Struct { children } = merge(a, b) else {
            panic!("expected Struct");
        };
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn merge_leaf_with_struct_yields_struct() {
        let a = ExplicitFields::Leaf;
        let b = ExplicitFields::Struct {
            children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
        };
        assert!(matches!(merge(a, b), ExplicitFields::Struct { .. }));
    }

    #[test]
    fn merge_recurses_into_nested_struct_children() {
        let a = ExplicitFields::Struct {
            children: HashMap::from([(
                "outer".into(),
                ExplicitFields::Struct {
                    children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
                },
            )]),
        };
        let b = ExplicitFields::Struct {
            children: HashMap::from([(
                "outer".into(),
                ExplicitFields::Struct {
                    children: HashMap::from([("b".into(), ExplicitFields::Leaf)]),
                },
            )]),
        };
        let ExplicitFields::Struct { children } = merge(a, b) else {
            panic!()
        };
        let ExplicitFields::Struct {
            children: inner, ..
        } = &children["outer"]
        else {
            panic!()
        };
        assert_eq!(inner.len(), 2);
    }
}
