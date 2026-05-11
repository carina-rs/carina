//! Shared diff computation helpers for plan display.
//!
//! These helpers extract the pure computation logic (no formatting/coloring)
//! that is shared between CLI and TUI frontends.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::resource::Value;

/// Count non-internal attributes that are semantically equal in both `from` and `to`.
///
/// Internal attributes (prefixed with `_`) are excluded from the count.
/// An optional `exclude` set can be provided to skip additional attribute names
/// (e.g., `changed_create_only` attributes in Replace effects).
pub fn compute_unchanged_count(
    from_attrs: &HashMap<String, Value>,
    to_attrs: &HashMap<String, Value>,
    exclude: Option<&std::collections::HashSet<&str>>,
) -> usize {
    from_attrs
        .iter()
        .filter(|(k, v)| {
            !k.starts_with('_')
                && exclude.is_none_or(|set| !set.contains(k.as_str()))
                && to_attrs
                    .get(k.as_str())
                    .map(|nv| nv.semantically_equal(v))
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

        assert_eq!(compute_unchanged_count(&from, &to, None), 2);
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

        assert_eq!(compute_unchanged_count(&from, &to, None), 1);
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
        assert_eq!(compute_unchanged_count(&from, &to, Some(&exclude)), 1);
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
