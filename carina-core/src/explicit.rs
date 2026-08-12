//! Per-resource user-authored field tree.
//!
//! `ExplicitFields` records which fields the user explicitly wrote in
//! their `.crn` for a resource. The differ projects the actual-state
//! through this tree before computing diffs so server-side default
//! fields the user never authored stop appearing as spurious removals.
//!
//! See `notes/specs/2026-05-10-explicit-fields-design.md`.

use crate::resource::{ConcreteValue, Resource, Value, pair_list_elements};
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
/// - `ListElements`: one authoring tree per element, indexed by the
///   stored list in the same state row.
/// - `Unrecorded`: no authoring record for this position. See the
///   per-variant doc on `Unrecorded` and carina#3280 for the
///   motivation — splitting it off from `Struct { children: {} }`
///   removes the runtime convention previously needed to
///   disambiguate "legacy-corrupt row" from "user wrote `{}`".
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
    /// Per-element authoring records indexed by the stored list value
    /// in the same state row. This does not provide identity across
    /// provider reads; plan-time list pairing selects the stored index.
    ListElements {
        elements: Vec<ExplicitFields>,
    },
    /// No authoring record for this position. Top-level callers
    /// (`project_attributes`) treat this as "pass attrs through, the
    /// authoring shape is whatever the user wrote in `.crn` right
    /// now"; recursive `project` treats it the same as `Leaf` (keep
    /// the entire value). Emitted by the `from_provider_state`
    /// legacy-corruption repair (`carina-state/src/state/mod.rs`)
    /// when the prior on-disk row carried an empty `Struct` whose
    /// children were destroyed by an older write-path bug; deserialised
    /// from `{ "kind": "unrecorded" }` in state files written after
    /// carina#3280.
    Unrecorded,
}

/// Build an `ExplicitFields::Struct` rooted at a resource's top-level
/// attributes. Underscore-prefixed keys (internal attributes) are
/// excluded. Lists are recorded in the resource value's order; state
/// writeback must use [`build_from_resource_for_stored_values`] instead.
#[cfg(test)]
pub(crate) fn build_from_resource(resource: &Resource) -> ExplicitFields {
    ExplicitFields::Struct {
        children: resource
            .attributes
            .iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), build_from_value(v)))
            .collect(),
    }
}

/// Build a resource authoring tree whose list records are aligned to
/// the provider-returned values that will be stored in the same row.
///
/// Desired lists without a concrete stored-list counterpart fall back
/// to the legacy union [`ExplicitFields::List`] representation rather
/// than recording indices that cannot be justified.
pub fn build_from_resource_for_stored_values(
    resource: &Resource,
    stored_attributes: &HashMap<String, Value>,
) -> ExplicitFields {
    ExplicitFields::Struct {
        children: resource
            .attributes
            .iter()
            .filter(|(key, _)| !key.starts_with('_'))
            .map(|(key, desired)| {
                (
                    key.clone(),
                    build_from_value_for_stored_value(desired, stored_attributes.get(key)),
                )
            })
            .collect(),
    }
}

/// Build an `ExplicitFields` tree describing the structural shape of a
/// `Value`. `Value::Concrete(ConcreteValue::Map)` is treated as a struct (each key becomes a
/// struct child); every `Value::Concrete(ConcreteValue::List)` becomes `ListElements` in the
/// value's own order; everything else is a `Leaf`.
pub fn build_from_value(value: &Value) -> ExplicitFields {
    match value {
        Value::Concrete(ConcreteValue::Map(fields)) => ExplicitFields::Struct {
            children: fields
                .iter()
                .map(|(k, v)| (k.clone(), build_from_value(v)))
                .collect(),
        },
        Value::Concrete(ConcreteValue::List(items)) => ExplicitFields::ListElements {
            elements: items.iter().map(build_from_value).collect(),
        },
        _ => ExplicitFields::Leaf,
    }
}

fn build_from_value_for_stored_value(desired: &Value, stored: Option<&Value>) -> ExplicitFields {
    match desired {
        Value::Concrete(ConcreteValue::Map(desired_fields)) => {
            let stored_fields = match stored {
                Some(Value::Concrete(ConcreteValue::Map(fields))) => Some(fields),
                _ => None,
            };
            ExplicitFields::Struct {
                children: desired_fields
                    .iter()
                    .map(|(key, desired_child)| {
                        (
                            key.clone(),
                            build_from_value_for_stored_value(
                                desired_child,
                                stored_fields.and_then(|fields| fields.get(key)),
                            ),
                        )
                    })
                    .collect(),
            }
        }
        Value::Concrete(ConcreteValue::List(desired_items)) => match stored {
            Some(Value::Concrete(ConcreteValue::List(stored_items))) => {
                let mut elements = vec![ExplicitFields::Unrecorded; stored_items.len()];
                for (desired_index, stored_index) in pair_list_elements(desired_items, stored_items)
                    .into_iter()
                    .enumerate()
                {
                    if let Some(stored_index) = stored_index {
                        elements[stored_index] = build_from_value_for_stored_value(
                            &desired_items[desired_index],
                            Some(&stored_items[stored_index]),
                        );
                    }
                }
                ExplicitFields::ListElements { elements }
            }
            _ => build_legacy_list(desired_items),
        },
        _ => ExplicitFields::Leaf,
    }
}

fn build_legacy_list(items: &[Value]) -> ExplicitFields {
    ExplicitFields::List {
        element: Box::new(
            items
                .iter()
                .map(build_from_value_conservatively)
                .fold(ExplicitFields::Leaf, merge),
        ),
    }
}

fn build_from_value_conservatively(value: &Value) -> ExplicitFields {
    match value {
        Value::Concrete(ConcreteValue::Map(fields)) => ExplicitFields::Struct {
            children: fields
                .iter()
                .map(|(key, value)| (key.clone(), build_from_value_conservatively(value)))
                .collect(),
        },
        Value::Concrete(ConcreteValue::List(items)) => build_legacy_list(items),
        _ => ExplicitFields::Leaf,
    }
}

fn union_elements(elements: impl IntoIterator<Item = ExplicitFields>) -> ExplicitFields {
    elements.into_iter().fold(ExplicitFields::Leaf, merge)
}

/// Conservatively discard every stored-index-aligned list vector in an authoring tree.
///
/// This is used when a fresh provider read replaces stored values but no desired values exist to
/// realign their authoring records. Each vector becomes its legacy union restriction, including
/// vectors nested inside structs and lists.
pub fn demote_list_elements_to_union(explicit: &ExplicitFields) -> ExplicitFields {
    match explicit {
        ExplicitFields::Leaf => ExplicitFields::Leaf,
        ExplicitFields::Unrecorded => ExplicitFields::Unrecorded,
        ExplicitFields::Struct { children } => ExplicitFields::Struct {
            children: children
                .iter()
                .map(|(key, child)| (key.clone(), demote_list_elements_to_union(child)))
                .collect(),
        },
        ExplicitFields::List { element } => ExplicitFields::List {
            element: Box::new(demote_list_elements_to_union(element)),
        },
        ExplicitFields::ListElements { elements } => ExplicitFields::List {
            element: Box::new(union_elements(
                elements.iter().map(demote_list_elements_to_union),
            )),
        },
    }
}

/// Combine two `ExplicitFields` trees by union semantics.
///
/// A root `ListElements` operand is always reduced to a legacy `List` union before it crosses this
/// seam, so this function never returns `ListElements` at the root. An unmatched nested struct
/// child can retain its existing shape; merge results are transient and are never persisted as row
/// authoring.
pub fn merge(a: ExplicitFields, b: ExplicitFields) -> ExplicitFields {
    use ExplicitFields::*;
    match (a, b) {
        (ListElements { elements: a }, ListElements { elements: b }) => List {
            element: Box::new(merge(union_elements(a), union_elements(b))),
        },
        (ListElements { elements }, List { element })
        | (List { element }, ListElements { elements }) => List {
            element: Box::new(merge(union_elements(elements), *element)),
        },
        (ListElements { elements }, Leaf | Unrecorded | Struct { .. })
        | (Leaf | Unrecorded | Struct { .. }, ListElements { elements }) => List {
            element: Box::new(union_elements(elements)),
        },
        (Leaf, b) => b,
        (a, Leaf) => a,
        // `Unrecorded` carries no shape information, so merging it
        // with anything yields the other side. `build_from_value`
        // never produces `Unrecorded` (only `from_provider_state`
        // does), so reaching this arm in production is unlikely —
        // the explicit handling exists to keep the `match` exhaustive
        // and prevent a future caller from getting a silent fallback
        // via the catch-all `(a, _) => a` arm below.
        (Unrecorded, b) => b,
        (a, Unrecorded) => a,
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
    match explicit {
        // user wrote whole leaf: keep entire current value
        ExplicitFields::Leaf => value,
        // no authoring record at this position: same effect as `Leaf`
        // — keep the entire current value (carina#3280).
        ExplicitFields::Unrecorded => value,
        ExplicitFields::Struct { children } => match value {
            Value::Concrete(ConcreteValue::Map(fields)) => {
                let projected: IndexMap<String, Value> = fields
                    .into_iter()
                    .filter_map(|(k, v)| children.get(&k).map(|sub| (k, project(v, sub))))
                    .collect();
                Value::Concrete(ConcreteValue::Map(projected))
            }
            // shape mismatch (state inconsistent or schema drift):
            // keep value as-is to avoid hiding real data.
            v => v,
        },
        ExplicitFields::List { element } => match value {
            Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
                items
                    .into_iter()
                    .map(|item| project(item, element))
                    .collect(),
            )),
            // shape mismatch: keep value as-is.
            v => v,
        },
        ExplicitFields::ListElements { elements } => {
            let union = union_elements(elements.iter().cloned());
            match value {
                Value::Concrete(ConcreteValue::List(items)) => {
                    Value::Concrete(ConcreteValue::List(
                        items
                            .into_iter()
                            .map(|item| project(item, &union))
                            .collect(),
                    ))
                }
                // shape mismatch: keep value as-is.
                v => v,
            }
        }
    }
}

/// Apply `project` to every entry of a top-level attribute map. The
/// outer `explicit` is expected to be `ExplicitFields::Struct` (the
/// shape the resource builders produce); `Unrecorded` (no authoring
/// record — emitted by the carina#3280 legacy-corruption repair)
/// passes attrs through unchanged. `Leaf` / `List` / `ListElements` at
/// the top level shouldn't occur for a resource's full attribute set
/// and pass through conservatively.
pub fn project_attributes(
    attrs: HashMap<String, Value>,
    explicit: &ExplicitFields,
) -> HashMap<String, Value> {
    match explicit {
        ExplicitFields::Struct { children } => attrs
            .into_iter()
            .filter_map(|(k, v)| children.get(&k).map(|sub| (k, project(v, sub))))
            .collect(),
        // No authoring record at the top level (carina#3280): the
        // legacy-corruption case where the differ must compare the
        // real `current` against `desired` directly, without
        // filtering. The `from_provider_state` repair rebuilds a
        // populated `Struct` on the next write.
        ExplicitFields::Unrecorded => attrs,
        // Top-level Leaf / List / ListElements shouldn't occur for a
        // resource's full attribute set; pass through conservatively.
        ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. } => attrs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{DeferredValue, UnknownReason};
    use indexmap::IndexMap;

    fn rule(port: i64, description: Option<&str>) -> Value {
        let mut fields = IndexMap::from([(
            "port".to_string(),
            Value::Concrete(ConcreteValue::Int(port)),
        )]);
        if let Some(description) = description {
            fields.insert(
                "description".to_string(),
                Value::Concrete(ConcreteValue::String(description.to_string())),
            );
        }
        Value::Concrete(ConcreteValue::Map(fields))
    }

    fn build_attribute_explicit(desired: Value, stored: Option<Value>) -> ExplicitFields {
        let resource =
            Resource::new("example.Listener", "listener").with_attribute("items", desired);
        let stored_attributes = stored
            .map(|stored| HashMap::from([("items".to_string(), stored)]))
            .unwrap_or_default();
        let ExplicitFields::Struct { mut children } =
            build_from_resource_for_stored_values(&resource, &stored_attributes)
        else {
            panic!("expected resource-root Struct");
        };
        children.remove("items").expect("items authoring")
    }

    fn assert_struct_keys(explicit: &ExplicitFields, expected: &[&str]) {
        let ExplicitFields::Struct { children } = explicit else {
            panic!("expected Struct, got {explicit:?}");
        };
        assert_eq!(children.len(), expected.len());
        for key in expected {
            assert!(children.contains_key(*key), "missing authored key {key}");
        }
    }

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
    fn list_elements_round_trips_with_exact_serde_kind() {
        let e = ExplicitFields::ListElements {
            elements: vec![ExplicitFields::Leaf, ExplicitFields::Unrecorded],
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"list-elements","elements":[{"kind":"leaf"},{"kind":"unrecorded"}]}"#
        );
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
    fn build_from_value_emits_list_elements_for_every_concrete_list_shape() {
        assert_eq!(
            build_from_value(&Value::Concrete(ConcreteValue::List(Vec::new()))),
            ExplicitFields::ListElements {
                elements: Vec::new()
            }
        );

        let scalar_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::String("two".to_string())),
        ]));
        assert_eq!(
            build_from_value(&scalar_list),
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Leaf, ExplicitFields::Leaf]
            }
        );

        let mut e1 = IndexMap::new();
        e1.insert("a".into(), Value::Concrete(ConcreteValue::Int(1)));
        e1.insert("b".into(), Value::Concrete(ConcreteValue::Int(1)));
        let mut e2 = IndexMap::new();
        e2.insert("b".into(), Value::Concrete(ConcreteValue::Int(2)));
        e2.insert("c".into(), Value::Concrete(ConcreteValue::Int(2)));
        let heterogeneous_maps = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(e1)),
            Value::Concrete(ConcreteValue::Map(e2)),
        ]));
        let ExplicitFields::ListElements { elements } = build_from_value(&heterogeneous_maps)
        else {
            panic!("expected ListElements");
        };
        assert_eq!(elements.len(), 2);
        let ExplicitFields::Struct { children: first } = &elements[0] else {
            panic!("expected first Struct element");
        };
        let ExplicitFields::Struct { children: second } = &elements[1] else {
            panic!("expected second Struct element");
        };
        assert_eq!(first.len(), 2);
        assert!(first.contains_key("a"));
        assert!(first.contains_key("b"));
        assert_eq!(second.len(), 2);
        assert!(second.contains_key("b"));
        assert!(second.contains_key("c"));

        let nested = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::List(vec![Value::Concrete(ConcreteValue::Int(1))]),
        )]));
        assert_eq!(
            build_from_value(&nested),
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::ListElements {
                    elements: vec![ExplicitFields::Leaf]
                }]
            }
        );

        assert_eq!(
            build_from_value(&Value::Concrete(ConcreteValue::StringList(vec![
                "one".to_string(),
                "two".to_string(),
            ]))),
            ExplicitFields::Leaf
        );
        assert_eq!(
            build_from_value(&Value::Deferred(DeferredValue::Unknown(
                UnknownReason::ForValue,
            ))),
            ExplicitFields::Leaf
        );
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
    fn aligned_builder_pairs_same_order_and_reordered_by_stored_index() {
        let desired = Value::Concrete(ConcreteValue::List(vec![
            rule(80, Some("web")),
            rule(443, None),
        ]));
        let same_order = build_attribute_explicit(
            desired.clone(),
            Some(Value::Concrete(ConcreteValue::List(vec![
                rule(80, Some("web")),
                rule(443, Some("provider-default")),
            ]))),
        );
        let ExplicitFields::ListElements { elements } = same_order else {
            panic!("expected aligned ListElements");
        };
        assert_struct_keys(&elements[0], &["port", "description"]);
        assert_struct_keys(&elements[1], &["port"]);

        let reordered = build_attribute_explicit(
            desired,
            Some(Value::Concrete(ConcreteValue::List(vec![
                rule(443, Some("provider-default")),
                rule(80, Some("web")),
            ]))),
        );
        let ExplicitFields::ListElements { elements } = reordered else {
            panic!("expected aligned ListElements");
        };
        assert_struct_keys(&elements[0], &["port"]);
        assert_struct_keys(&elements[1], &["port", "description"]);
    }

    #[test]
    fn aligned_builder_handles_extra_missing_zero_score_and_duplicate_elements() {
        let desired = Value::Concrete(ConcreteValue::List(vec![
            rule(80, Some("web")),
            rule(443, None),
        ]));
        let extra = build_attribute_explicit(
            desired.clone(),
            Some(Value::Concrete(ConcreteValue::List(vec![
                rule(443, Some("provider-default")),
                rule(22, Some("provider-added")),
                rule(80, Some("web")),
            ]))),
        );
        let ExplicitFields::ListElements { elements } = extra else {
            panic!("expected aligned ListElements");
        };
        assert_eq!(elements.len(), 3);
        assert_struct_keys(&elements[0], &["port"]);
        assert_eq!(elements[1], ExplicitFields::Unrecorded);
        assert_struct_keys(&elements[2], &["port", "description"]);

        let missing = build_attribute_explicit(
            desired,
            Some(Value::Concrete(ConcreteValue::List(vec![rule(
                443,
                Some("provider-default"),
            )]))),
        );
        let ExplicitFields::ListElements { elements } = missing else {
            panic!("expected aligned ListElements");
        };
        assert_eq!(elements.len(), 1);
        assert_struct_keys(&elements[0], &["port"]);

        let no_match = build_attribute_explicit(
            Value::Concrete(ConcreteValue::List(vec![rule(80, None)])),
            Some(Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([(
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("unrelated".to_string())),
                )])),
            )]))),
        );
        assert_eq!(
            no_match,
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Unrecorded]
            }
        );

        let duplicate = build_attribute_explicit(
            Value::Concrete(ConcreteValue::List(vec![
                rule(80, Some("web")),
                rule(80, Some("web")),
            ])),
            Some(Value::Concrete(ConcreteValue::List(vec![
                rule(80, Some("web")),
                rule(80, Some("web")),
            ]))),
        );
        let ExplicitFields::ListElements { elements } = duplicate else {
            panic!("expected aligned ListElements");
        };
        assert_struct_keys(&elements[0], &["port", "description"]);
        assert_struct_keys(&elements[1], &["port", "description"]);
    }

    #[test]
    fn aligned_builder_empty_desired_list_records_unrecorded_for_every_stored_element() {
        let aligned = build_attribute_explicit(
            Value::Concrete(ConcreteValue::List(Vec::new())),
            Some(Value::Concrete(ConcreteValue::List(vec![
                rule(80, Some("provider-default")),
                rule(443, Some("provider-default")),
            ]))),
        );

        assert_eq!(
            aligned,
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Unrecorded, ExplicitFields::Unrecorded],
            }
        );
    }

    #[test]
    fn aligned_builder_deferred_unknown_desired_value_is_leaf() {
        let aligned = build_attribute_explicit(
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            Some(Value::Concrete(ConcreteValue::List(vec![rule(
                80,
                Some("provider-default"),
            )]))),
        );

        assert_eq!(aligned, ExplicitFields::Leaf);
    }

    #[test]
    fn aligned_builder_recurses_into_nested_lists_and_falls_back_to_legacy_union() {
        let nested_desired = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([
                ("id".to_string(), Value::Concrete(ConcreteValue::Int(1))),
                (
                    "nested".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![
                        rule(80, Some("web")),
                        rule(443, None),
                    ])),
                ),
            ])),
        )]));
        let nested_stored = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([
                ("id".to_string(), Value::Concrete(ConcreteValue::Int(1))),
                (
                    "nested".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![
                        rule(443, Some("provider-default")),
                        rule(80, Some("web")),
                    ])),
                ),
            ])),
        )]));
        let nested = build_attribute_explicit(nested_desired, Some(nested_stored));
        let ExplicitFields::ListElements { elements } = nested else {
            panic!("expected outer ListElements");
        };
        let ExplicitFields::Struct { children } = &elements[0] else {
            panic!("expected outer Struct");
        };
        let ExplicitFields::ListElements {
            elements: inner_elements,
        } = &children["nested"]
        else {
            panic!("expected nested ListElements");
        };
        assert_struct_keys(&inner_elements[0], &["port"]);
        assert_struct_keys(&inner_elements[1], &["port", "description"]);

        for stored in [
            None,
            Some(Value::Concrete(ConcreteValue::String(
                "shape-mismatch".to_string(),
            ))),
        ] {
            let fallback = build_attribute_explicit(
                Value::Concrete(ConcreteValue::List(vec![
                    rule(80, Some("web")),
                    rule(443, None),
                ])),
                stored,
            );
            let ExplicitFields::List { element } = fallback else {
                panic!("missing or mismatched stored list must use legacy List");
            };
            assert_struct_keys(&element, &["port", "description"]);
        }

        let nested_fallback = build_attribute_explicit(
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(IndexMap::from([(
                    "nested".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![rule(80, None)])),
                )])),
            )])),
            None,
        );
        let ExplicitFields::List { element } = nested_fallback else {
            panic!("expected conservative outer List");
        };
        let ExplicitFields::Struct { children } = &*element else {
            panic!("expected conservative element Struct");
        };
        assert!(matches!(children["nested"], ExplicitFields::List { .. }));
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
    fn merge_reduces_every_list_elements_combination_to_legacy_union() {
        let fields = |keys: &[&str]| ExplicitFields::Struct {
            children: keys
                .iter()
                .map(|key| ((*key).to_string(), ExplicitFields::Leaf))
                .collect(),
        };
        let list = |keys: &[&str]| ExplicitFields::List {
            element: Box::new(fields(keys)),
        };
        let elements = ExplicitFields::ListElements {
            elements: vec![fields(&["a"]), fields(&["b"])],
        };
        let cases = vec![
            (
                ExplicitFields::ListElements {
                    elements: vec![fields(&["c"])],
                },
                list(&["a", "b", "c"]),
            ),
            (list(&["d"]), list(&["a", "b", "d"])),
            (ExplicitFields::Leaf, list(&["a", "b"])),
            (ExplicitFields::Unrecorded, list(&["a", "b"])),
            (fields(&["malformed"]), list(&["a", "b"])),
        ];

        for (other, expected) in cases {
            let left = merge(elements.clone(), other.clone());
            let right = merge(other, elements.clone());
            assert_eq!(left, expected);
            assert_eq!(right, expected);
            assert!(!matches!(left, ExplicitFields::ListElements { .. }));
            assert!(!matches!(right, ExplicitFields::ListElements { .. }));
        }
    }

    #[test]
    fn merge_can_preserve_list_elements_in_an_unmatched_nested_struct_child() {
        let nested = ExplicitFields::ListElements {
            elements: vec![ExplicitFields::Struct {
                children: HashMap::from([("id".to_string(), ExplicitFields::Leaf)]),
            }],
        };
        let merged = merge(
            ExplicitFields::Struct {
                children: HashMap::from([("a".to_string(), nested.clone())]),
            },
            ExplicitFields::Struct {
                children: HashMap::from([("b".to_string(), ExplicitFields::Leaf)]),
            },
        );

        let ExplicitFields::Struct { children } = merged else {
            panic!("expected root Struct");
        };
        assert_eq!(children["a"], nested);
    }

    #[test]
    fn demote_list_elements_to_union_recursively_removes_root_and_nested_vectors() {
        let aligned = ExplicitFields::ListElements {
            elements: vec![
                ExplicitFields::Struct {
                    children: HashMap::from([
                        ("id".to_string(), ExplicitFields::Leaf),
                        (
                            "nested".to_string(),
                            ExplicitFields::ListElements {
                                elements: vec![ExplicitFields::Struct {
                                    children: HashMap::from([(
                                        "value".to_string(),
                                        ExplicitFields::Leaf,
                                    )]),
                                }],
                            },
                        ),
                    ]),
                },
                ExplicitFields::Unrecorded,
            ],
        };

        let demoted = demote_list_elements_to_union(&aligned);
        let ExplicitFields::List { element } = demoted else {
            panic!("expected root legacy List");
        };
        let ExplicitFields::Struct { children } = &*element else {
            panic!("expected unioned Struct element");
        };
        assert!(matches!(children["nested"], ExplicitFields::List { .. }));

        fn contains_list_elements(explicit: &ExplicitFields) -> bool {
            match explicit {
                ExplicitFields::ListElements { .. } => true,
                ExplicitFields::Struct { children } => {
                    children.values().any(contains_list_elements)
                }
                ExplicitFields::List { element } => contains_list_elements(element),
                ExplicitFields::Leaf | ExplicitFields::Unrecorded => false,
            }
        }
        assert!(!contains_list_elements(element.as_ref()));
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
    fn project_unrecorded_keeps_whole_value() {
        // carina#3280: `Unrecorded` at a recursive `project` position
        // is treated like `Leaf` — the whole value is kept. Top-level
        // `project_attributes` exercises the same semantic for the
        // full attribute map; this direct unit test pins the recursive
        // behaviour so a future refactor of `project` cannot regress
        // it via the `Leaf | Unrecorded` arm consolidation.
        let mut fields = IndexMap::new();
        fields.insert("any".into(), Value::Concrete(ConcreteValue::Int(1)));
        let value = Value::Concrete(ConcreteValue::Map(fields));
        let result = project(value.clone(), &ExplicitFields::Unrecorded);
        assert_eq!(result, value);
    }

    #[test]
    fn unrecorded_round_trips_via_serde() {
        // carina#3280: `Unrecorded` must serialize as
        // `{"kind":"unrecorded"}` (kebab-case via
        // `rename_all = "kebab-case"`, no inner fields) and round-trip
        // back to the same variant. The fixture
        // `empty_explicit_children_no_changes/carina.state.json`
        // depends on this spelling.
        let e = ExplicitFields::Unrecorded;
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(json, r#"{"kind":"unrecorded"}"#);
        let back: ExplicitFields = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
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
    fn project_list_variants_preserve_indices_and_apply_one_union_to_every_item() {
        let item = |sentinel: &str| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "sentinel".to_string(),
                    Value::Concrete(ConcreteValue::String(sentinel.to_string())),
                ),
                (
                    "authored_a".to_string(),
                    Value::Concrete(ConcreteValue::Int(1)),
                ),
                (
                    "authored_b".to_string(),
                    Value::Concrete(ConcreteValue::Int(2)),
                ),
                ("server".to_string(), Value::Concrete(ConcreteValue::Int(3))),
            ])))
        };
        let value = Value::Concrete(ConcreteValue::List(vec![
            item("third"),
            item("first"),
            item("second"),
        ]));
        let union = ExplicitFields::Struct {
            children: HashMap::from([
                ("sentinel".to_string(), ExplicitFields::Leaf),
                ("authored_a".to_string(), ExplicitFields::Leaf),
                ("authored_b".to_string(), ExplicitFields::Leaf),
            ]),
        };
        let legacy = ExplicitFields::List {
            element: Box::new(union),
        };
        let per_element = ExplicitFields::ListElements {
            elements: vec![
                ExplicitFields::Struct {
                    children: HashMap::from([
                        ("sentinel".to_string(), ExplicitFields::Leaf),
                        ("authored_a".to_string(), ExplicitFields::Leaf),
                    ]),
                },
                ExplicitFields::Struct {
                    children: HashMap::from([
                        ("sentinel".to_string(), ExplicitFields::Leaf),
                        ("authored_b".to_string(), ExplicitFields::Leaf),
                    ]),
                },
            ],
        };

        let legacy_projected = project(value.clone(), &legacy);
        let per_element_projected = project(value, &per_element);
        assert_eq!(legacy_projected, per_element_projected);

        let Value::Concrete(ConcreteValue::List(items)) = per_element_projected else {
            panic!("expected projected list");
        };
        assert_eq!(items.len(), 3);
        let sentinels: Vec<&str> = items
            .iter()
            .map(|item| {
                let Value::Concrete(ConcreteValue::Map(fields)) = item else {
                    panic!("expected projected map");
                };
                assert_eq!(fields.len(), 3);
                assert!(!fields.contains_key("server"));
                let Some(Value::Concrete(ConcreteValue::String(sentinel))) = fields.get("sentinel")
                else {
                    panic!("expected sentinel string");
                };
                sentinel.as_str()
            })
            .collect();
        assert_eq!(sentinels, vec!["third", "first", "second"]);
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
    fn project_attributes_passes_through_when_unrecorded() {
        // carina#3280: a legacy state row with no authoring record (a
        // for-loop child persisted before the expansion path populated
        // attributes correctly). `Unrecorded` is the typed signal —
        // pass the attrs through so downstream equality can compare
        // them against `desired`. Distinct from `Struct { children: {} }`
        // which means "user authored an empty struct here".
        let attrs = HashMap::from([
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            (
                "b".to_string(),
                Value::Concrete(ConcreteValue::String("x".into())),
            ),
        ]);
        let result = project_attributes(attrs.clone(), &ExplicitFields::Unrecorded);
        assert_eq!(result, attrs);
    }

    #[test]
    fn project_attributes_drops_top_level_unauthored_when_struct_children_empty() {
        // Counterpart to `project_attributes_passes_through_when_unrecorded`:
        // `Struct { children: {} }` is *not* the "no record" signal —
        // it means "user authored an empty struct at this position" —
        // and projection correctly drops every attribute. The two
        // shapes are structurally distinct so callers no longer have
        // to disambiguate at runtime (carina#3280).
        let attrs = HashMap::from([("a".to_string(), Value::Concrete(ConcreteValue::Int(1)))]);
        let explicit = ExplicitFields::Struct {
            children: HashMap::new(),
        };
        let result = project_attributes(attrs, &explicit);
        assert!(result.is_empty(), "empty Struct must drop all attrs");
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
