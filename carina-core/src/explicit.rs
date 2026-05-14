//! Per-resource user-authored field tree.
//!
//! `ExplicitFields` records which fields the user explicitly wrote in
//! their `.crn` for a resource. The differ projects the actual-state
//! through this tree before computing diffs so server-side default
//! fields the user never authored stop appearing as spurious removals.
//!
//! See `notes/specs/2026-05-10-explicit-fields-design.md`.

use crate::resource::{ConcreteValue, Resource, Value};
use indexmap::IndexMap;
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
/// `Value`. `Value::Concrete(ConcreteValue::Map)` is treated as a struct (each key becomes a
/// struct child); `Value::Concrete(ConcreteValue::List)` becomes a `List` whose element is the
/// union of authoring across all elements; everything else is a `Leaf`.
pub fn build_from_value(value: &Value) -> ExplicitFields {
    match value {
        Value::Concrete(ConcreteValue::Map(fields)) => ExplicitFields::Struct {
            children: fields
                .iter()
                .map(|(k, v)| (k.clone(), build_from_value(v)))
                .collect(),
        },
        Value::Concrete(ConcreteValue::List(items)) => ExplicitFields::List {
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

/// Strip from `value` everything not listed in `explicit`. Used to
/// remove server-side defaults from the actual-state side before
/// diffing.
///
/// Idempotent: `project(project(v, e), e) == project(v, e)`.
///
/// Shape-mismatch fallback: when `value` and `explicit` disagree on
/// shape (e.g. `Value::Concrete(ConcreteValue::String)` paired with `ExplicitFields::Struct`),
/// the value is returned unchanged. This is a conservative choice —
/// better to over-show a value once than to silently hide real data.
pub fn project(value: Value, explicit: &ExplicitFields) -> Value {
    match (value, explicit) {
        // user wrote whole leaf: keep entire current value
        (v, ExplicitFields::Leaf) => v,
        (Value::Concrete(ConcreteValue::Map(fields)), ExplicitFields::Struct { children }) => {
            let projected: IndexMap<String, Value> = fields
                .into_iter()
                .filter_map(|(k, v)| children.get(&k).map(|sub| (k, project(v, sub))))
                .collect();
            Value::Concrete(ConcreteValue::Map(projected))
        }
        (Value::Concrete(ConcreteValue::List(items)), ExplicitFields::List { element }) => {
            Value::Concrete(ConcreteValue::List(
                items
                    .into_iter()
                    .map(|item| project(item, element))
                    .collect(),
            ))
        }
        // shape mismatch (state inconsistent or schema drift): keep
        // value as-is to avoid hiding real data
        (v, _) => v,
    }
}

/// Apply `project` to every entry of a top-level attribute map. The
/// outer `explicit` is expected to be `ExplicitFields::Struct` (the
/// shape `build_from_resource` produces); other variants pass through
/// conservatively.
pub fn project_attributes(
    attrs: HashMap<String, Value>,
    explicit: &ExplicitFields,
) -> HashMap<String, Value> {
    match explicit {
        ExplicitFields::Struct { children } => attrs
            .into_iter()
            .filter_map(|(k, v)| children.get(&k).map(|sub| (k, project(v, sub))))
            .collect(),
        // Top-level being Leaf or List shouldn't occur for a
        // resource's full attribute set; pass through conservatively.
        _ => attrs,
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
        let v = Value::Concrete(ConcreteValue::String("x".into()));
        assert_eq!(build_from_value(&v), ExplicitFields::Leaf);
    }

    #[test]
    fn build_from_value_struct_collects_children() {
        let mut fields = IndexMap::new();
        fields.insert(
            "a".into(),
            Value::Concrete(ConcreteValue::String("x".into())),
        );
        fields.insert("b".into(), Value::Concrete(ConcreteValue::Int(1)));
        let v = Value::Concrete(ConcreteValue::Map(fields));
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
        e1.insert("a".into(), Value::Concrete(ConcreteValue::Int(1)));
        e1.insert("b".into(), Value::Concrete(ConcreteValue::Int(1)));
        let mut e2 = IndexMap::new();
        e2.insert("b".into(), Value::Concrete(ConcreteValue::Int(2)));
        e2.insert("c".into(), Value::Concrete(ConcreteValue::Int(2)));
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(e1)),
            Value::Concrete(ConcreteValue::Map(e2)),
        ]));
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
        let mut r = Resource::with_provider("aws", "s3.Bucket", "x", None);
        r.set_attr(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("hi".into())),
        );
        r.set_attr(
            "_internal".to_string(),
            Value::Concrete(ConcreteValue::String("skip".into())),
        );
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
    fn project_struct_drops_unauthored_field() {
        let mut fields = IndexMap::new();
        fields.insert(
            "authored".into(),
            Value::Concrete(ConcreteValue::String("keep".into())),
        );
        fields.insert(
            "server_default".into(),
            Value::Concrete(ConcreteValue::String("drop".into())),
        );
        let value = Value::Concrete(ConcreteValue::Map(fields));
        let explicit = ExplicitFields::Struct {
            children: HashMap::from([("authored".into(), ExplicitFields::Leaf)]),
        };
        let Value::Concrete(ConcreteValue::Map(projected)) = project(value, &explicit) else {
            panic!("expected Map");
        };
        assert_eq!(projected.len(), 1);
        assert!(projected.contains_key("authored"));
        assert!(!projected.contains_key("server_default"));
    }

    #[test]
    fn project_leaf_keeps_whole_value() {
        let mut fields = IndexMap::new();
        fields.insert("any".into(), Value::Concrete(ConcreteValue::Int(1)));
        let value = Value::Concrete(ConcreteValue::Map(fields));
        let result = project(value.clone(), &ExplicitFields::Leaf);
        assert_eq!(result, value);
    }

    #[test]
    fn project_is_idempotent() {
        let mut fields = IndexMap::new();
        fields.insert("a".into(), Value::Concrete(ConcreteValue::Int(1)));
        fields.insert("b".into(), Value::Concrete(ConcreteValue::Int(2)));
        let value = Value::Concrete(ConcreteValue::Map(fields));
        let explicit = ExplicitFields::Struct {
            children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
        };
        let once = project(value, &explicit);
        let twice = project(once.clone(), &explicit);
        assert_eq!(once, twice);
    }

    #[test]
    fn project_list_recurses_into_each_element() {
        let mut e1 = IndexMap::new();
        e1.insert("authored".into(), Value::Concrete(ConcreteValue::Int(1)));
        e1.insert("server".into(), Value::Concrete(ConcreteValue::Int(2)));
        let mut e2 = IndexMap::new();
        e2.insert("authored".into(), Value::Concrete(ConcreteValue::Int(3)));
        e2.insert("server".into(), Value::Concrete(ConcreteValue::Int(4)));
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(e1)),
            Value::Concrete(ConcreteValue::Map(e2)),
        ]));
        let explicit = ExplicitFields::List {
            element: Box::new(ExplicitFields::Struct {
                children: HashMap::from([("authored".into(), ExplicitFields::Leaf)]),
            }),
        };
        let Value::Concrete(ConcreteValue::List(items)) = project(value, &explicit) else {
            panic!("expected List");
        };
        assert_eq!(items.len(), 2);
        for item in &items {
            let Value::Concrete(ConcreteValue::Map(fields)) = item else {
                panic!("expected Map element");
            };
            assert_eq!(fields.len(), 1);
            assert!(fields.contains_key("authored"));
        }
    }

    #[test]
    fn project_mismatched_shape_keeps_value() {
        // Authoring says Struct, value is a String — keep value as-is.
        let value = Value::Concrete(ConcreteValue::String("oops".into()));
        let explicit = ExplicitFields::Struct {
            children: HashMap::new(),
        };
        let result = project(value.clone(), &explicit);
        assert_eq!(result, value);
    }

    #[test]
    fn project_attributes_drops_top_level_unauthored() {
        let attrs = HashMap::from([
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            (
                "server_only".to_string(),
                Value::Concrete(ConcreteValue::Int(99)),
            ),
        ]);
        let explicit = ExplicitFields::Struct {
            children: HashMap::from([("a".into(), ExplicitFields::Leaf)]),
        };
        let result = project_attributes(attrs, &explicit);
        assert_eq!(result.len(), 1);
        assert!(result.contains_key("a"));
    }

    #[test]
    fn project_attributes_passes_through_when_explicit_is_leaf() {
        let attrs = HashMap::from([("a".to_string(), Value::Concrete(ConcreteValue::Int(1)))]);
        let result = project_attributes(attrs.clone(), &ExplicitFields::Leaf);
        assert_eq!(result, attrs);
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
