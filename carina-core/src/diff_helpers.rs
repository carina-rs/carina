//! Shared diff computation helpers for plan display.
//!
//! These helpers extract the pure computation logic (no formatting/coloring)
//! that is shared between CLI and TUI frontends.

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

use crate::resource::Value;
use crate::schema::{AttributeType, ResourceSchema, empty_defs_for_schema_walks};

/// Schema-aware value equality shared by the plan renderer
/// (`detail_rows`) and the unchanged-count helper below (carina#3073).
///
/// With a resolved `attr_type`, delegate to the differ's exact
/// `type_aware_equal` (its `Enum` arm alias-folds
/// `EnumIdentifier("allow")` vs `String("Allow")`), so the rendered
/// rows and the hidden-count agree with `find_changed_attributes`.
/// Without one (no registry — embedded / test callers) fall back to
/// the schema-blind `Value::semantically_equal`, leaving that path's
/// behavior unchanged. `secret_ctx` is `None`: a
/// `Value::Deferred(Secret(_))` would short-circuit
/// `type_aware_equal`'s secret arm and compare unequal, which only ever
/// *over*-reports a diff (safe-by-default) — it never hides a real
/// change.
pub(crate) fn schema_aware_equal(
    old: &Value,
    new: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> bool {
    match attr_type {
        Some(t) => crate::differ::type_aware_equal(old, new, Some(t), defs, None),
        None => old.semantically_equal(new),
    }
}

/// Resolve the subtype for map entry `key`: a `Map`'s `value` type, a
/// `Struct` field's type, or `None`. This is a field-search walk, so it
/// deliberately peels every enclosing `List` until it reaches a `Map` or
/// `Struct`; nested `List<Map>`/`List<Struct>` types therefore still resolve
/// their entries (carina#3073). This differs intentionally from
/// [`list_element_type`], which consumes exactly one list boundary. Uses the
/// canonical `build_accepted_field_map` so a `block_name`-aliased struct field
/// resolves like `validate_struct`.
///
/// `defs` is the enclosing schema's `defs` map; any [`AttributeType::Ref`]
/// reached during unwrap is peeled against it. Without this, a
/// `Ref`-typed attribute returns `None` for every entry and schema-aware
/// equality silently degrades to its schema-blind fallback.
pub(crate) fn map_entry_subtype<'a>(
    attr_type: Option<&'a AttributeType>,
    key: &str,
    defs: &'a BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let mut t = attr_type?;
    loop {
        match t.shape_with_defs(defs) {
            crate::schema::Shape::List {
                element_type: inner,
                ..
            } => t = inner,
            crate::schema::Shape::Map { value, .. } => return Some(value),
            crate::schema::Shape::Struct { .. } => {
                let fields = crate::schema::struct_fields_with_defs(t, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                return crate::schema::build_accepted_field_map(fields)
                    .get(key)
                    .map(|field| &field.field_type);
            }
            crate::schema::Shape::Union => {
                t = union_member_for_walk(t, defs, UnionWalkTarget::MapEntry)?;
            }
            _ => return None,
        }
    }
}

#[derive(Clone, Copy)]
enum UnionWalkTarget {
    MapEntry,
    ListElement,
}

/// Select a Union member that has the shape required by the current walk.
///
/// List-element walks require a `List` member. Map-entry walks prefer a direct
/// `Struct`/`Map` member because they are resolving a field by key, then fall
/// back to a `List` wrapper so shapes such as `Union[String, List<Struct>]`
/// continue to work. Selection is therefore independent of member declaration
/// order when a Union contains both a map-like and a list member. This helper
/// shares only Union-member selection; each caller retains its own traversal
/// depth because the two walks answer different questions.
fn union_member_for_walk<'a>(
    attr_type: &'a AttributeType,
    defs: &'a BTreeMap<String, AttributeType>,
    target: UnionWalkTarget,
) -> Option<&'a AttributeType> {
    let members = crate::schema::union_members_with_defs(attr_type, defs)
        .expect("Shape::Union must expose union members internally");
    match target {
        UnionWalkTarget::ListElement => members.iter().find(|member| {
            matches!(
                member.shape_with_defs(defs),
                crate::schema::Shape::List { .. }
            )
        }),
        UnionWalkTarget::MapEntry => members
            .iter()
            .find(|member| {
                matches!(
                    member.shape_with_defs(defs),
                    crate::schema::Shape::Struct { .. } | crate::schema::Shape::Map { .. }
                )
            })
            .or_else(|| {
                members.iter().find(|member| {
                    matches!(
                        member.shape_with_defs(defs),
                        crate::schema::Shape::List { .. }
                    )
                })
            }),
    }
}

/// Return the immediate element type for a list-shaped attribute.
///
/// Callers that are already walking a list element may pass the element type
/// itself; in that case it is returned unchanged. Leading `Ref` nodes are
/// always peeled with the enclosing schema's definitions. For a `Union`, the
/// List member is selected regardless of declaration order, then its immediate
/// element type is returned. Nested lists are not flattened: recursive callers
/// invoke this helper once for each `Value::List` boundary they cross.
pub(crate) fn list_element_type<'a>(
    attr_type: Option<&'a AttributeType>,
    defs: &'a BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let mut current = attr_type?;
    loop {
        match current.shape_with_defs(defs) {
            crate::schema::Shape::List { element_type, .. } => return Some(element_type),
            crate::schema::Shape::Union => {
                current = union_member_for_walk(current, defs, UnionWalkTarget::ListElement)?;
            }
            _ => return Some(current.resolve_refs_with_defs(defs).as_attr()),
        }
    }
}

/// Count non-internal attributes that are equal in both `from` and `to`.
///
/// Internal attributes (prefixed with `_`) are excluded from the count.
/// An optional `exclude` set can be provided to skip additional attribute names
/// (e.g., `changed_create_only` attributes in Replace effects).
///
/// When `schema` is provided, equality is schema-aware (carina#3073):
/// an attribute whose only difference is an enum-equal leaf
/// (`EnumIdentifier("allow")` vs `String("Allow")`) is counted as
/// **unchanged** — matching the renderer, which now suppresses its
/// phantom row via the same `type_aware_equal`. Without that, such an
/// attribute would render no row *and* not be counted, so the Full-mode
/// `# (n unchanged attributes hidden)` tally would not add up. With
/// `schema = None`, behavior is the schema-blind `semantically_equal`
/// path (unchanged for embedded / test callers).
pub fn compute_unchanged_count(
    from_attrs: &HashMap<String, Value>,
    to_attrs: &HashMap<String, Value>,
    exclude: Option<&std::collections::HashSet<&str>>,
    schema: Option<&ResourceSchema>,
) -> usize {
    let defs: &BTreeMap<String, AttributeType> = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());
    from_attrs
        .iter()
        .filter(|(k, v)| {
            !k.starts_with('_')
                && exclude.is_none_or(|set| !set.contains(k.as_str()))
                && to_attrs
                    .get(k.as_str())
                    .map(|nv| {
                        let attr_type = schema
                            .and_then(|s| s.attributes.get(k.as_str()))
                            .map(|a| &a.attr_type);
                        schema_aware_equal(nv, v, attr_type, defs)
                    })
                    .unwrap_or(false)
        })
        .count()
}

/// Result of computing a map diff between two maps.
#[derive(Debug, Clone, PartialEq)]
pub struct MapDiff {
    /// Keys added in the new map (sorted).
    pub added: Vec<MapDiffEntry>,
    /// Keys removed from the old map (sorted).
    pub removed: Vec<MapDiffEntry>,
    /// Keys present in both but with different values (sorted).
    pub changed: Vec<MapDiffChanged>,
}

/// A single added or removed map entry.
#[derive(Debug, Clone, PartialEq)]
pub struct MapDiffEntry {
    pub key: String,
    pub value: Value,
}

/// A changed map entry with old and new values.
#[derive(Debug, Clone, PartialEq)]
pub struct MapDiffChanged {
    pub key: String,
    pub old_value: Value,
    pub new_value: Value,
}

/// A reference to a single diff entry, used when iterating in key order.
#[derive(Debug)]
pub enum MapDiffItem<'a> {
    Added(&'a MapDiffEntry),
    Removed(&'a MapDiffEntry),
    Changed(&'a MapDiffChanged),
}

impl MapDiff {
    /// Iterate over all diff entries in sorted key order.
    ///
    /// This merges added, removed, and changed entries and yields them
    /// sorted by key, matching the original interleaved output order.
    pub fn iter_by_key(&self) -> Vec<MapDiffItem<'_>> {
        let mut items: Vec<(String, MapDiffItem<'_>)> = Vec::new();
        for e in &self.added {
            items.push((e.key.clone(), MapDiffItem::Added(e)));
        }
        for e in &self.removed {
            items.push((e.key.clone(), MapDiffItem::Removed(e)));
        }
        for e in &self.changed {
            items.push((e.key.clone(), MapDiffItem::Changed(e)));
        }
        items.sort_by(|(a, _), (b, _)| a.cmp(b));
        items.into_iter().map(|(_, item)| item).collect()
    }
}

/// Compute the diff between two maps, returning added, removed, and changed entries.
///
/// All result vectors are sorted by key for deterministic output, so the
/// caller's input order does not affect the result.
pub fn compute_map_diff(
    old_map: &IndexMap<String, Value>,
    new_map: &IndexMap<String, Value>,
) -> MapDiff {
    let mut all_keys: Vec<&String> = old_map.keys().chain(new_map.keys()).collect();
    all_keys.sort();
    all_keys.dedup();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();

    for key in all_keys {
        let old_val = old_map.get(key);
        let new_val = new_map.get(key);
        match (old_val, new_val) {
            (Some(ov), Some(nv)) => {
                if !ov.semantically_equal(nv) {
                    changed.push(MapDiffChanged {
                        key: key.clone(),
                        old_value: ov.clone(),
                        new_value: nv.clone(),
                    });
                }
            }
            (None, Some(nv)) => {
                added.push(MapDiffEntry {
                    key: key.clone(),
                    value: nv.clone(),
                });
            }
            (Some(ov), None) => {
                removed.push(MapDiffEntry {
                    key: key.clone(),
                    value: ov.clone(),
                });
            }
            (None, None) => {}
        }
    }

    MapDiff {
        added,
        removed,
        changed,
    }
}

/// Diff result for two `Vec<String>` slices, partitioned by value
/// membership. `unchanged` follows new-list order; `added` and
/// `removed` preserve their source order.
#[derive(Debug, Clone, PartialEq)]
pub struct StringListDiff {
    pub unchanged: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// Diff two `&[String]` slices into `unchanged` / `added` / `removed`.
/// Set semantics: equality is by value, not position, and duplicate
/// elements are conflated.
pub fn compute_string_list_diff(old: &[String], new: &[String]) -> StringListDiff {
    use std::collections::HashSet;
    let old_set: HashSet<&str> = old.iter().map(String::as_str).collect();
    let new_set: HashSet<&str> = new.iter().map(String::as_str).collect();

    let mut unchanged = Vec::new();
    let mut added = Vec::new();
    for s in new {
        if old_set.contains(s.as_str()) {
            unchanged.push(s.clone());
        } else {
            added.push(s.clone());
        }
    }
    let mut removed = Vec::new();
    for s in old {
        if !new_set.contains(s.as_str()) {
            removed.push(s.clone());
        }
    }
    StringListDiff {
        unchanged,
        added,
        removed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ConcreteValue;
    use crate::schema::StructField;

    fn struct_or_list_union(struct_first: bool) -> AttributeType {
        let struct_member = AttributeType::struct_(
            "Entry",
            vec![StructField::new("enabled", AttributeType::bool())],
        );
        let list_member = AttributeType::list(AttributeType::string());
        AttributeType::union(if struct_first {
            vec![struct_member, list_member]
        } else {
            vec![list_member, struct_member]
        })
    }

    #[test]
    fn list_element_type_selects_list_member_regardless_of_union_order() {
        let defs = empty_defs_for_schema_walks();
        for struct_first in [true, false] {
            let attr_type = struct_or_list_union(struct_first);
            let element = list_element_type(Some(&attr_type), defs)
                .expect("Union contains a List element type");
            assert!(
                matches!(
                    element.shape_with_defs(defs),
                    crate::schema::Shape::String { .. }
                ),
                "expected String list element with struct_first={struct_first}, got {:?}",
                element.shape_with_defs(defs)
            );
        }
    }

    #[test]
    fn map_entry_subtype_selects_struct_member_regardless_of_union_order() {
        let defs = empty_defs_for_schema_walks();
        for struct_first in [true, false] {
            let attr_type = struct_or_list_union(struct_first);
            let field_type = map_entry_subtype(Some(&attr_type), "enabled", defs)
                .expect("Union contains a Struct with the requested field");
            assert!(
                matches!(field_type.shape_with_defs(defs), crate::schema::Shape::Bool),
                "expected Bool field type with struct_first={struct_first}, got {:?}",
                field_type.shape_with_defs(defs)
            );
        }
    }

    #[test]
    fn nested_list_helpers_keep_one_level_element_and_field_search_semantics() {
        let defs = empty_defs_for_schema_walks();
        let attr_type = AttributeType::list(AttributeType::list(AttributeType::struct_(
            "Entry",
            vec![StructField::new("sid", AttributeType::string())],
        )));

        let immediate_element =
            list_element_type(Some(&attr_type), defs).expect("outer List has an element type");
        assert!(
            matches!(
                immediate_element.shape_with_defs(defs),
                crate::schema::Shape::List { .. }
            ),
            "list-element lookup must consume only the outer List"
        );
        let nested_element = list_element_type(Some(immediate_element), defs)
            .expect("inner List has an element type");
        assert!(
            matches!(
                nested_element.shape_with_defs(defs),
                crate::schema::Shape::Struct { .. }
            ),
            "a second value-list recursion must reach the Struct"
        );

        let field_type = map_entry_subtype(Some(&attr_type), "sid", defs)
            .expect("field search must cross both List wrappers");
        assert!(matches!(
            field_type.shape_with_defs(defs),
            crate::schema::Shape::String { .. }
        ));
    }

    #[test]
    fn list_element_type_unwraps_union_typed_string_or_list_of_strings() {
        let attr_type = AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::list(AttributeType::string()),
        ]);
        let defs = empty_defs_for_schema_walks();

        let element = list_element_type(Some(&attr_type), defs).expect("Union contains a List");
        assert!(matches!(
            element.shape_with_defs(defs),
            crate::schema::Shape::String { .. }
        ));
    }

    #[test]
    fn test_compute_unchanged_count_basic() {
        let from: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "region".to_string(),
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            ),
            ("size".to_string(), Value::Concrete(ConcreteValue::Int(10))),
        ]
        .into_iter()
        .collect();

        let to: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "region".to_string(),
                Value::Concrete(ConcreteValue::String("us-west-2".to_string())),
            ),
            ("size".to_string(), Value::Concrete(ConcreteValue::Int(10))),
        ]
        .into_iter()
        .collect();

        assert_eq!(compute_unchanged_count(&from, &to, None, None), 2);
    }

    #[test]
    fn test_compute_unchanged_count_excludes_internal() {
        let from: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "_internal".to_string(),
                Value::Concrete(ConcreteValue::String("hidden".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let to: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "_internal".to_string(),
                Value::Concrete(ConcreteValue::String("hidden".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(compute_unchanged_count(&from, &to, None, None), 1);
    }

    #[test]
    fn test_compute_unchanged_count_with_exclude_set() {
        let from: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "region".to_string(),
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let to: HashMap<String, Value> = [
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".to_string())),
            ),
            (
                "region".to_string(),
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let exclude: std::collections::HashSet<&str> = ["region"].into_iter().collect();
        assert_eq!(compute_unchanged_count(&from, &to, Some(&exclude), None), 1);
    }

    #[test]
    fn test_compute_map_diff_added_only() {
        let old: IndexMap<String, Value> = IndexMap::new();
        let new: IndexMap<String, Value> = [
            (
                "key1".to_string(),
                Value::Concrete(ConcreteValue::String("val1".to_string())),
            ),
            (
                "key2".to_string(),
                Value::Concrete(ConcreteValue::String("val2".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let diff = compute_map_diff(&old, &new);
        assert_eq!(diff.added.len(), 2);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.changed.len(), 0);
        assert_eq!(diff.added[0].key, "key1");
        assert_eq!(diff.added[1].key, "key2");
    }

    #[test]
    fn test_compute_map_diff_removed_only() {
        let old: IndexMap<String, Value> = [(
            "key1".to_string(),
            Value::Concrete(ConcreteValue::String("val1".to_string())),
        )]
        .into_iter()
        .collect();
        let new: IndexMap<String, Value> = IndexMap::new();

        let diff = compute_map_diff(&old, &new);
        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.changed.len(), 0);
        assert_eq!(diff.removed[0].key, "key1");
    }

    #[test]
    fn test_compute_map_diff_changed() {
        let old: IndexMap<String, Value> = [
            (
                "key1".to_string(),
                Value::Concrete(ConcreteValue::String("old_val".to_string())),
            ),
            (
                "key2".to_string(),
                Value::Concrete(ConcreteValue::String("same".to_string())),
            ),
        ]
        .into_iter()
        .collect();
        let new: IndexMap<String, Value> = [
            (
                "key1".to_string(),
                Value::Concrete(ConcreteValue::String("new_val".to_string())),
            ),
            (
                "key2".to_string(),
                Value::Concrete(ConcreteValue::String("same".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let diff = compute_map_diff(&old, &new);
        assert_eq!(diff.added.len(), 0);
        assert_eq!(diff.removed.len(), 0);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].key, "key1");
        assert_eq!(
            diff.changed[0].old_value,
            Value::Concrete(ConcreteValue::String("old_val".to_string()))
        );
        assert_eq!(
            diff.changed[0].new_value,
            Value::Concrete(ConcreteValue::String("new_val".to_string()))
        );
    }

    #[test]
    fn test_compute_map_diff_mixed() {
        let old: IndexMap<String, Value> = [
            (
                "keep".to_string(),
                Value::Concrete(ConcreteValue::String("same".to_string())),
            ),
            (
                "change".to_string(),
                Value::Concrete(ConcreteValue::String("old".to_string())),
            ),
            (
                "remove".to_string(),
                Value::Concrete(ConcreteValue::String("gone".to_string())),
            ),
        ]
        .into_iter()
        .collect();
        let new: IndexMap<String, Value> = [
            (
                "keep".to_string(),
                Value::Concrete(ConcreteValue::String("same".to_string())),
            ),
            (
                "change".to_string(),
                Value::Concrete(ConcreteValue::String("new".to_string())),
            ),
            (
                "add".to_string(),
                Value::Concrete(ConcreteValue::String("fresh".to_string())),
            ),
        ]
        .into_iter()
        .collect();

        let diff = compute_map_diff(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].key, "add");
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].key, "remove");
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].key, "change");
    }
}
