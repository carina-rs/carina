//! Output-neutral intermediate representation for plan detail rows.
//!
//! Both CLI and TUI consume `DetailRow` values to render effect details.
//! The `build_detail_rows` function encapsulates all logic for deciding
//! which rows to show for a given effect, keeping rendering frontends thin.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::diff_helpers::{compute_map_diff, compute_unchanged_count};
use crate::effect::Effect;
use crate::resource::{ResourceId, Value};
use crate::schema::SchemaRegistry;
use crate::value::{format_value, format_value_with_key, is_list_of_maps, map_similarity};

/// Controls how much detail is shown in plan output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLevel {
    /// Show all attributes: user-specified, defaults, read-only, and unchanged (dimmed)
    Full,
    /// Show only attributes explicitly specified in .crn file
    Explicit,
    /// Show resource names only (no attributes)
    NamesOnly,
}

/// Output-neutral representation of a detail row for plan display.
/// Both CLI and TUI consume these to render effect details.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailRow {
    /// A normal attribute value (for Create/Delete effects)
    Attribute {
        key: String,
        value: String,
        /// If this attribute is a ResourceRef, stores the binding name for navigation
        ref_binding: Option<String>,
        /// Optional annotation (e.g., "# default_tags") shown after the value
        annotation: Option<String>,
    },
    /// An expanded map attribute with per-entry annotations (e.g., tags with default_tags)
    MapExpanded {
        key: String,
        entries: Vec<MapExpandedEntry>,
    },
    /// An attribute whose value is rendered with `format_value_pretty` at
    /// render time. Used for list-of-map attributes on Create — carries the
    /// raw `Value` rather than a pre-stringified form so the renderer can
    /// supply the actual indent column when calling `format_value_pretty`.
    PrettyAttribute { key: String, value: Value },
    /// An attribute that changed (for Update effects)
    Changed {
        key: String,
        old: String,
        new: String,
    },
    /// A map attribute with key-level diffs (for Update effects)
    MapDiff {
        key: String,
        entries: Vec<MapDiffEntryIR>,
    },
    /// A list-of-maps diff (for Update effects)
    ListOfMapsDiff {
        key: String,
        unchanged: Vec<String>,
        modified: Vec<ListOfMapsDiffModified>,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// An attribute that was removed (for Update effects)
    Removed { key: String, old: String },
    /// An attribute with a schema default value (for Create effects in Full mode)
    Default { key: String, value: String },
    /// A read-only attribute (known after apply, for Create effects in Full mode)
    ReadOnly { key: String },
    /// Summary of hidden unchanged attributes (for Update/Replace effects in Full mode)
    HiddenUnchanged { count: usize },
    /// A changed create-only attribute that forces replacement
    ReplaceChanged {
        key: String,
        old: String,
        new: String,
    },
    /// A cascade-triggered create-only attribute (value not yet known)
    ReplaceCascade {
        key: String,
        old: String,
        new: String,
    },
    /// A list-of-maps diff that forces replacement
    ReplaceListOfMapsDiff {
        key: String,
        unchanged: Vec<String>,
        modified: Vec<ListOfMapsDiffModified>,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// A map diff that forces replacement
    ReplaceMapDiff {
        key: String,
        entries: Vec<MapDiffEntryIR>,
    },
    /// Temporary name note for create-before-destroy replacement
    TemporaryNameNote {
        can_rename: bool,
        temporary_value: String,
        original_value: String,
        attribute: String,
    },
    /// Cascading updates section
    CascadingUpdates {
        count: usize,
        updates: Vec<CascadingUpdateIR>,
    },
}

/// A single entry in an expanded map (e.g., tags with default_tags annotation)
#[derive(Debug, Clone, PartialEq)]
pub struct MapExpandedEntry {
    pub key: String,
    pub value: String,
    /// Optional annotation (e.g., "# default_tags") shown after the value
    pub annotation: Option<String>,
}

/// A map diff entry (key-level diff)
#[derive(Debug, Clone, PartialEq)]
pub enum MapDiffEntryIR {
    Added {
        key: String,
        value: String,
    },
    Removed {
        key: String,
        value: String,
    },
    Changed {
        key: String,
        old: String,
        new: String,
    },
    /// Nested map diff: when both old and new values are maps,
    /// recursively compute key-level diffs instead of showing as one-liner.
    NestedMapDiff {
        key: String,
        entries: Vec<MapDiffEntryIR>,
    },
    /// Nested list-of-maps diff: when both old and new values are lists of maps,
    /// show per-item field-level diffs instead of one-liner.
    NestedListOfMapsDiff {
        key: String,
        modified: Vec<ListOfMapsDiffModified>,
        added: Vec<String>,
        removed: Vec<String>,
    },
}

/// A modified item in a list-of-maps diff
#[derive(Debug, Clone, PartialEq)]
pub struct ListOfMapsDiffModified {
    /// Ordered list of fields, each either unchanged or changed
    pub fields: Vec<ListOfMapsDiffField>,
}

/// A single field in a modified list-of-maps item
#[derive(Debug, Clone, PartialEq)]
pub enum ListOfMapsDiffField {
    /// Field value is unchanged
    Unchanged { key: String, value: String },
    /// Field value changed
    Changed {
        key: String,
        old: String,
        new: String,
    },
    /// Field value is a nested map that changed — show recursive key-level diffs
    NestedMapChanged {
        key: String,
        entries: Vec<MapDiffEntryIR>,
    },
}

/// A cascading update entry
#[derive(Debug, Clone, PartialEq)]
pub struct CascadingUpdateIR {
    pub display_type: String,
    pub name: String,
    pub changed_attrs: Vec<CascadingUpdateAttr>,
}

/// A changed attribute in a cascading update
#[derive(Debug, Clone, PartialEq)]
pub struct CascadingUpdateAttr {
    pub key: String,
    pub old: String,
    pub new: String,
}

/// Build detail rows for an effect.
///
/// This function encapsulates ALL the logic for deciding what detail rows to
/// show for a given effect. The caller only needs to render each `DetailRow`
/// with appropriate formatting (colors, prefixes, etc.).
pub fn build_detail_rows(
    effect: &Effect,
    registry: Option<&SchemaRegistry>,
    detail: DetailLevel,
    delete_attributes: Option<&HashMap<ResourceId, HashMap<String, Value>>>,
) -> Vec<DetailRow> {
    if detail == DetailLevel::NamesOnly {
        return Vec::new();
    }

    match effect {
        Effect::Create(r) => build_create_rows(r, registry, detail),
        Effect::Update {
            from,
            to,
            changed_attributes,
            ..
        } => build_update_rows(from, to, changed_attributes, detail),
        Effect::Replace {
            from,
            to,
            changed_create_only,
            cascading_updates,
            temporary_name,
            cascade_ref_hints,
            ..
        } => build_replace_rows(
            from,
            to,
            changed_create_only,
            cascading_updates,
            temporary_name,
            cascade_ref_hints,
            detail,
        ),
        Effect::Delete { id, .. } => build_delete_rows(id, delete_attributes),
        Effect::Read { resource } => build_create_rows(resource, registry, detail),
        Effect::Import { identifier, .. } => {
            vec![DetailRow::Attribute {
                key: "id".to_string(),
                value: identifier.clone(),
                ref_binding: None,
                annotation: None,
            }]
        }
        Effect::Remove { .. } | Effect::Move { .. } => Vec::new(),
    }
}

fn build_create_rows(
    r: &crate::resource::Resource,
    registry: Option<&SchemaRegistry>,
    detail: DetailLevel,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    // Collect default_tag_keys for annotation
    let default_tag_keys: HashSet<String> = r
        .attributes
        .get("_default_tag_keys")
        .and_then(|v| match v {
            Value::List(items) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    let mut keys: Vec<_> = r
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    for key in &keys {
        let value = &r.attributes[*key];
        // Expand tags map into individual rows with default_tags annotation
        if key.as_str() == "tags"
            && !default_tag_keys.is_empty()
            && let Value::Map(map) = value
        {
            rows.push(build_expanded_tags_row(map, &default_tag_keys));
            continue;
        }
        // `Value::List` (any element type) goes through PrettyAttribute so
        // `format_value_pretty` can apply its 80-col threshold and YAML-style
        // vertical layout. `Value::Map` keeps the existing MapExpanded path
        // because that variant carries per-entry annotation slots that
        // PrettyAttribute does not represent (used by tags/default_tags).
        if let Value::List(_) = value {
            rows.push(DetailRow::PrettyAttribute {
                key: key.to_string(),
                value: value.clone(),
            });
        } else if let Value::Map(map) = value {
            rows.push(build_expanded_map_row(key, map));
        } else {
            let ref_binding = match value {
                Value::ResourceRef { path } => Some(path.binding().to_string()),
                _ => None,
            };
            rows.push(DetailRow::Attribute {
                key: key.to_string(),
                value: format_value_with_key(value, Some(key)),
                ref_binding,
                annotation: None,
            });
        }
    }

    // In Full mode, show defaults and read-only attributes
    if detail == DetailLevel::Full
        && let Some(registry) = registry
        && let Some(schema) = registry.get_for(r)
    {
        let user_keys: HashSet<&str> = keys.iter().map(|k| k.as_str()).collect();

        for (attr, formatted) in schema.compute_default_attrs(&user_keys) {
            rows.push(DetailRow::Default {
                key: attr,
                value: formatted,
            });
        }

        for attr in schema.compute_read_only_attrs(&user_keys) {
            rows.push(DetailRow::ReadOnly { key: attr });
        }
    }

    rows
}

/// Build a `DetailRow::MapExpanded` for a map attribute (no annotations).
fn build_expanded_map_row(key: &str, map: &IndexMap<String, Value>) -> DetailRow {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let entries = keys
        .into_iter()
        .map(|k| MapExpandedEntry {
            key: k.clone(),
            value: format_value_with_key(&map[k], Some(k)),
            annotation: None,
        })
        .collect();
    DetailRow::MapExpanded {
        key: key.to_string(),
        entries,
    }
}

/// Build a `DetailRow::MapExpanded` for tags with `default_tags` annotations.
fn build_expanded_tags_row(
    map: &IndexMap<String, Value>,
    default_tag_keys: &HashSet<String>,
) -> DetailRow {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let entries = keys
        .into_iter()
        .map(|key| {
            let value = &map[key];
            let annotation = if default_tag_keys.contains(key) {
                Some("# default_tags".to_string())
            } else {
                None
            };
            MapExpandedEntry {
                key: key.clone(),
                value: format_value_with_key(value, Some(key)),
                annotation,
            }
        })
        .collect();
    DetailRow::MapExpanded {
        key: "tags".to_string(),
        entries,
    }
}

fn build_update_rows(
    from: &crate::resource::State,
    to: &crate::resource::Resource,
    changed_attributes: &[String],
    detail: DetailLevel,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    let mut keys: Vec<_> = to
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    for key in keys {
        let new_value = &to.attributes[key];
        let old_value = from.attributes.get(key);
        let is_same = old_value
            .map(|ov| ov.semantically_equal(new_value))
            .unwrap_or(false);

        if is_same {
            continue;
        }

        if is_list_of_maps(new_value) {
            rows.push(build_list_of_maps_diff_row(key, old_value, new_value));
        } else if is_both_maps(old_value, new_value) {
            rows.push(build_map_diff_row(key, old_value, new_value));
        } else {
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            rows.push(DetailRow::Changed {
                key: key.to_string(),
                old: old_str,
                new: format_value_with_key(new_value, Some(key)),
            });
        }
    }

    // Show removed attributes (in changed_attributes but not in to)
    let mut removed_keys: Vec<_> = changed_attributes
        .iter()
        .filter(|k| !to.attributes.contains_key(k.as_str()))
        .collect();
    removed_keys.sort();
    for key in removed_keys {
        if let Some(old_value) = from.attributes.get(key.as_str()) {
            rows.push(DetailRow::Removed {
                key: key.to_string(),
                old: format_value_with_key(old_value, Some(key)),
            });
        }
    }

    // In Full mode, show count of unchanged attributes hidden
    if detail == DetailLevel::Full {
        let unchanged_count =
            compute_unchanged_count(&from.attributes, &to.resolved_attributes(), None);
        if unchanged_count > 0 {
            rows.push(DetailRow::HiddenUnchanged {
                count: unchanged_count,
            });
        }
    }

    rows
}

fn build_replace_rows(
    from: &crate::resource::State,
    to: &crate::resource::Resource,
    changed_create_only: &[String],
    cascading_updates: &[crate::effect::CascadingUpdate],
    temporary_name: &Option<crate::effect::TemporaryName>,
    cascade_ref_hints: &[(String, String)],
    detail: DetailLevel,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    // Show changed create-only attributes
    let mut keys: Vec<_> = changed_create_only
        .iter()
        .filter(|k| to.attributes.contains_key(k.as_str()))
        .collect();
    keys.sort();

    for key in keys {
        let new_value = &to.attributes[key.as_str()];
        let old_value = from.attributes.get(key.as_str());
        let is_same = old_value
            .map(|ov| ov.semantically_equal(new_value))
            .unwrap_or(false);

        if is_same {
            // Cascade-triggered: value not yet known
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            let new_str = cascade_ref_hints
                .iter()
                .find(|(attr, _)| attr == key)
                .map(|(_, hint)| hint.clone())
                .unwrap_or_else(|| format_value_with_key(new_value, Some(key)));
            rows.push(DetailRow::ReplaceCascade {
                key: key.to_string(),
                old: old_str,
                new: new_str,
            });
        } else if is_list_of_maps(new_value) {
            let (unchanged, modified, added, removed) =
                compute_list_of_maps_diff_parts(old_value, new_value);
            rows.push(DetailRow::ReplaceListOfMapsDiff {
                key: key.to_string(),
                unchanged,
                modified,
                added,
                removed,
            });
        } else if is_both_maps(old_value, new_value) {
            let entries = compute_map_diff_entries(old_value, new_value);
            rows.push(DetailRow::ReplaceMapDiff {
                key: key.to_string(),
                entries,
            });
        } else {
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            rows.push(DetailRow::ReplaceChanged {
                key: key.to_string(),
                old: old_str,
                new: format_value_with_key(new_value, Some(key)),
            });
        }
    }

    // Temporary name note
    if let Some(temp) = temporary_name {
        rows.push(DetailRow::TemporaryNameNote {
            can_rename: temp.can_rename,
            temporary_value: temp.temporary_value.clone(),
            original_value: temp.original_value.clone(),
            attribute: temp.attribute.clone(),
        });
    }

    // In Full mode, show count of unchanged attributes hidden
    if detail == DetailLevel::Full {
        let changed_set: HashSet<&str> = changed_create_only.iter().map(|s| s.as_str()).collect();
        let unchanged_count = compute_unchanged_count(
            &from.attributes,
            &to.resolved_attributes(),
            Some(&changed_set),
        );
        if unchanged_count > 0 {
            rows.push(DetailRow::HiddenUnchanged {
                count: unchanged_count,
            });
        }
    }

    // Cascading updates
    if !cascading_updates.is_empty() {
        let replaced_binding = to.binding.as_deref().unwrap_or("");

        let mut updates = Vec::new();
        for cascade in cascading_updates {
            let mut changed_attrs = Vec::new();
            let mut cascade_keys: Vec<_> = cascade
                .to
                .attributes
                .keys()
                .filter(|k| !k.starts_with('_'))
                .collect();
            cascade_keys.sort();

            for key in cascade_keys {
                let new_value = &cascade.to.attributes[key];
                if !value_references_binding(new_value, replaced_binding) {
                    continue;
                }
                let old_value = cascade.from.attributes.get(key);
                let old_str = old_value
                    .map(|v| format_value_with_key(v, Some(key)))
                    .unwrap_or_else(|| "(none)".to_string());
                let new_str = format_value_with_key(new_value, Some(key));
                changed_attrs.push(CascadingUpdateAttr {
                    key: key.to_string(),
                    old: old_str,
                    new: new_str,
                });
            }

            updates.push(CascadingUpdateIR {
                display_type: cascade.id.display_type(),
                name: cascade.id.name_str().to_string(),
                changed_attrs,
            });
        }

        rows.push(DetailRow::CascadingUpdates {
            count: cascading_updates.len(),
            updates,
        });
    }

    rows
}

fn build_delete_rows(
    id: &ResourceId,
    delete_attributes: Option<&HashMap<ResourceId, HashMap<String, Value>>>,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    if let Some(attrs) = delete_attributes.and_then(|da| da.get(id)) {
        let mut keys: Vec<_> = attrs.keys().filter(|k| !k.starts_with('_')).collect();
        keys.sort();
        for key in keys {
            let value = &attrs[key];
            if let Value::Map(map) = value {
                rows.push(build_expanded_map_row(key, map));
            } else {
                let ref_binding = match value {
                    Value::ResourceRef { path } => Some(path.binding().to_string()),
                    _ => None,
                };
                rows.push(DetailRow::Attribute {
                    key: key.to_string(),
                    value: format_value_with_key(value, Some(key)),
                    ref_binding,
                    annotation: None,
                });
            }
        }
    }

    rows
}

fn build_map_diff_row(key: &str, old_value: Option<&Value>, new_value: &Value) -> DetailRow {
    let entries = compute_map_diff_entries(old_value, new_value);
    DetailRow::MapDiff {
        key: key.to_string(),
        entries,
    }
}

fn compute_map_diff_entries(old_value: Option<&Value>, new_value: &Value) -> Vec<MapDiffEntryIR> {
    let new_map = match new_value {
        Value::Map(m) => m,
        _ => return Vec::new(),
    };
    let old_map = match old_value {
        Some(Value::Map(m)) => m,
        _ => {
            let empty: IndexMap<String, Value> = IndexMap::new();
            let diff = compute_map_diff(&empty, new_map);
            return diff
                .added
                .iter()
                .map(|entry| MapDiffEntryIR::Added {
                    key: entry.key.clone(),
                    value: format_value_with_key(&entry.value, Some(&entry.key)),
                })
                .collect();
        }
    };

    let diff = compute_map_diff(old_map, new_map);
    let mut entries = Vec::new();

    for item in diff.iter_by_key() {
        match item {
            crate::diff_helpers::MapDiffItem::Changed(e) => {
                // If both old and new are maps, recursively diff
                if matches!(&e.old_value, Value::Map(_)) && matches!(&e.new_value, Value::Map(_)) {
                    let nested = compute_map_diff_entries(Some(&e.old_value), &e.new_value);
                    entries.push(MapDiffEntryIR::NestedMapDiff {
                        key: e.key.clone(),
                        entries: nested,
                    });
                } else if is_list_of_maps(&e.new_value) {
                    // List-of-maps: compute per-item field-level diffs
                    let (_, modified, added, removed) =
                        compute_list_of_maps_diff_parts(Some(&e.old_value), &e.new_value);
                    entries.push(MapDiffEntryIR::NestedListOfMapsDiff {
                        key: e.key.clone(),
                        modified,
                        added,
                        removed,
                    });
                } else {
                    entries.push(MapDiffEntryIR::Changed {
                        key: e.key.clone(),
                        old: format_value_with_key(&e.old_value, Some(&e.key)),
                        new: format_value_with_key(&e.new_value, Some(&e.key)),
                    });
                }
            }
            crate::diff_helpers::MapDiffItem::Added(e) => {
                entries.push(MapDiffEntryIR::Added {
                    key: e.key.clone(),
                    value: format_value_with_key(&e.value, Some(&e.key)),
                });
            }
            crate::diff_helpers::MapDiffItem::Removed(e) => {
                entries.push(MapDiffEntryIR::Removed {
                    key: e.key.clone(),
                    value: format_value_with_key(&e.value, Some(&e.key)),
                });
            }
        }
    }

    entries
}

fn build_list_of_maps_diff_row(
    key: &str,
    old_value: Option<&Value>,
    new_value: &Value,
) -> DetailRow {
    let (unchanged, modified, added, removed) =
        compute_list_of_maps_diff_parts(old_value, new_value);
    DetailRow::ListOfMapsDiff {
        key: key.to_string(),
        unchanged,
        modified,
        added,
        removed,
    }
}

fn compute_list_of_maps_diff_parts(
    old_value: Option<&Value>,
    new_value: &Value,
) -> (
    Vec<String>,
    Vec<ListOfMapsDiffModified>,
    Vec<String>,
    Vec<String>,
) {
    let new_items = match new_value {
        Value::List(items) => items,
        _ => return (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
    };
    let old_items = match old_value {
        Some(Value::List(items)) => items,
        _ => &vec![] as &Vec<Value>,
    };

    let mut old_matched = vec![false; old_items.len()];
    let mut new_matched = vec![false; new_items.len()];

    // Phase 1: Find exact matches
    for (ni, new_item) in new_items.iter().enumerate() {
        for (oi, old_item) in old_items.iter().enumerate() {
            if !old_matched[oi] && old_item.semantically_equal(new_item) {
                old_matched[oi] = true;
                new_matched[ni] = true;
                break;
            }
        }
    }

    let unmatched_old: Vec<usize> = old_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();
    let unmatched_new: Vec<usize> = new_matched
        .iter()
        .enumerate()
        .filter(|(_, m)| !**m)
        .map(|(i, _)| i)
        .collect();

    // Phase 2: Pair unmatched items by similarity
    let mut paired: Vec<(usize, usize)> = Vec::new();
    let mut paired_old = vec![false; unmatched_old.len()];
    let mut paired_new = vec![false; unmatched_new.len()];

    for (ui_new, &ni) in unmatched_new.iter().enumerate() {
        let mut best_oi_idx = None;
        let mut best_sim = 0usize;
        for (ui_old, &oi) in unmatched_old.iter().enumerate() {
            if paired_old[ui_old] {
                continue;
            }
            let sim = map_similarity(&old_items[oi], &new_items[ni]);
            if sim > best_sim {
                best_sim = sim;
                best_oi_idx = Some(ui_old);
            }
        }
        if let Some(ui_old) = best_oi_idx.filter(|_| best_sim > 0) {
            paired.push((unmatched_old[ui_old], ni));
            paired_old[ui_old] = true;
            paired_new[ui_new] = true;
        }
    }

    let added_indices: Vec<usize> = unmatched_new
        .iter()
        .enumerate()
        .filter(|(i, _)| !paired_new[*i])
        .map(|(_, &ni)| ni)
        .collect();
    let removed_indices: Vec<usize> = unmatched_old
        .iter()
        .enumerate()
        .filter(|(i, _)| !paired_old[*i])
        .map(|(_, &oi)| oi)
        .collect();

    // Build output parts
    let mut unchanged = Vec::new();
    for (ni, new_item) in new_items.iter().enumerate() {
        if let Value::Map(map) = new_item
            && new_matched[ni]
        {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            unchanged.push(format!("{{{}}}", fields.join(", ")));
        }
    }

    let mut modified = Vec::new();
    for &(oi, ni) in &paired {
        if let (Value::Map(old_map), Value::Map(new_map)) = (&old_items[oi], &new_items[ni]) {
            let mut keys: Vec<_> = new_map.keys().collect();
            keys.sort();
            let fields: Vec<ListOfMapsDiffField> = keys
                .iter()
                .map(|k| {
                    let new_v = format_value(&new_map[*k]);
                    let field_same = old_map
                        .get(*k)
                        .map(|ov| ov.semantically_equal(&new_map[*k]))
                        .unwrap_or(false);
                    if !field_same {
                        let old_val = old_map.get(*k);
                        // If both old and new are maps, show recursive diff
                        if matches!(old_val, Some(Value::Map(_)))
                            && matches!(&new_map[*k], Value::Map(_))
                        {
                            let nested = compute_map_diff_entries(old_val, &new_map[*k]);
                            ListOfMapsDiffField::NestedMapChanged {
                                key: k.to_string(),
                                entries: nested,
                            }
                        } else {
                            let old_v = old_val
                                .map(format_value)
                                .unwrap_or_else(|| "(none)".to_string());
                            ListOfMapsDiffField::Changed {
                                key: k.to_string(),
                                old: old_v,
                                new: new_v,
                            }
                        }
                    } else {
                        ListOfMapsDiffField::Unchanged {
                            key: k.to_string(),
                            value: new_v,
                        }
                    }
                })
                .collect();
            modified.push(ListOfMapsDiffModified { fields });
        }
    }

    let mut added = Vec::new();
    for &ni in &added_indices {
        if let Value::Map(map) = &new_items[ni] {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            added.push(format!("{{{}}}", fields.join(", ")));
        }
    }

    let mut removed = Vec::new();
    for &oi in &removed_indices {
        if let Value::Map(map) = &old_items[oi] {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            removed.push(format!("{{{}}}", fields.join(", ")));
        }
    }

    (unchanged, modified, added, removed)
}

/// Check if both old and new values are `Value::Map`.
fn is_both_maps(old_value: Option<&Value>, new_value: &Value) -> bool {
    matches!((old_value, new_value), (Some(Value::Map(_)), Value::Map(_)))
}

/// Check whether a Value references the given binding name.
fn value_references_binding(value: &Value, binding: &str) -> bool {
    let mut found = false;
    value.visit_refs(&mut |path| {
        if path.binding() == binding {
            found = true;
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Resource, ResourceId, State};
    use std::collections::HashSet;

    #[test]
    fn test_names_only_returns_empty() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::NamesOnly, None);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_create_basic_attributes() {
        let resource = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("bucket", Value::String("my-bucket".to_string()))
            .with_attribute("region", Value::String("us-east-1".to_string()));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], DetailRow::Attribute { key, .. } if key == "bucket"));
        assert!(matches!(&rows[1], DetailRow::Attribute { key, .. } if key == "region"));
    }

    #[test]
    fn test_update_changed_attributes() {
        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [(
                "versioning".to_string(),
                Value::String("Disabled".to_string()),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("versioning", Value::String("Enabled".to_string()));
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["versioning".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Changed { key, old, new }
            if key == "versioning" && old == "\"Disabled\"" && new == "\"Enabled\""));
    }

    #[test]
    fn test_update_hidden_unchanged_in_full_mode() {
        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [
                ("name".to_string(), Value::String("test".to_string())),
                ("region".to_string(), Value::String("us-east-1".to_string())),
                (
                    "versioning".to_string(),
                    Value::String("Disabled".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("name", Value::String("test".to_string()))
            .with_attribute("region", Value::String("us-east-1".to_string()))
            .with_attribute("versioning", Value::String("Enabled".to_string()));
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["versioning".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None);
        // 1 changed + 1 hidden unchanged (2 unchanged attrs)
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], DetailRow::Changed { .. }));
        assert!(matches!(&rows[1], DetailRow::HiddenUnchanged { count: 2 }));
    }

    #[test]
    fn test_delete_with_attributes() {
        let id = ResourceId::new("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: id.clone(),
            identifier: "old-bucket".to_string(),
            lifecycle: crate::resource::LifecycleConfig::default(),
            binding: None,
            dependencies: HashSet::new(),
        };
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [(
                "bucket".to_string(),
                Value::String("old-bucket".to_string()),
            )]
            .into_iter()
            .collect(),
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs));
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Attribute { key, .. } if key == "bucket"));
    }

    #[test]
    fn test_update_removed_attribute() {
        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [
                ("name".to_string(), Value::String("test".to_string())),
                (
                    "removed_attr".to_string(),
                    Value::String("old-val".to_string()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("name", Value::String("test".to_string()));
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["removed_attr".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Removed { key, .. } if key == "removed_attr"));
    }

    #[test]
    fn test_create_map_expanded() {
        let mut tags = IndexMap::new();
        tags.insert("Name".to_string(), Value::String("test".to_string()));
        tags.insert("Environment".to_string(), Value::String("prod".to_string()));
        let resource =
            Resource::new("s3.Bucket", "my-bucket").with_attribute("tags", Value::Map(tags));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            DetailRow::MapExpanded { key, entries } => {
                assert_eq!(key, "tags");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key, "Environment");
                assert_eq!(entries[0].value, "\"prod\"");
                assert!(entries[0].annotation.is_none());
                assert_eq!(entries[1].key, "Name");
                assert_eq!(entries[1].value, "\"test\"");
                assert!(entries[1].annotation.is_none());
            }
            other => panic!("expected MapExpanded, got {:?}", other),
        }
    }

    #[test]
    fn test_delete_map_expanded() {
        let id = ResourceId::new("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: id.clone(),
            identifier: "old-bucket".to_string(),
            lifecycle: crate::resource::LifecycleConfig::default(),
            binding: None,
            dependencies: HashSet::new(),
        };
        let mut tags = IndexMap::new();
        tags.insert("Name".to_string(), Value::String("test".to_string()));
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [("tags".to_string(), Value::Map(tags))]
                .into_iter()
                .collect(),
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs));
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            DetailRow::MapExpanded { key, entries } => {
                assert_eq!(key, "tags");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, "Name");
                assert_eq!(entries[0].value, "\"test\"");
            }
            other => panic!("expected MapExpanded, got {:?}", other),
        }
    }

    #[test]
    fn test_replace_basic() {
        let from = State::existing(
            ResourceId::new("ec2.Vpc", "my-vpc"),
            [(
                "cidr_block".to_string(),
                Value::String("10.0.0.0/16".to_string()),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("ec2.Vpc", "my-vpc")
            .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));
        let effect = Effect::Replace {
            id: ResourceId::new("ec2.Vpc", "my-vpc"),
            from: Box::new(from),
            to,
            lifecycle: crate::resource::LifecycleConfig::default(),
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::ReplaceChanged { key, .. } if key == "cidr_block"));
    }

    /// The pre-#1971 hand-rolled walker only descended into `List` and `Map`,
    /// so a ResourceRef nested inside `Interpolation` / `FunctionCall` /
    /// `Secret` / `Closure` would fail to mark the attribute as referencing
    /// the binding. Guard against regressing that after migrating to
    /// `Value::visit_refs`.
    #[test]
    fn value_references_binding_covers_non_container_variants() {
        use crate::resource::InterpolationPart;

        let refs_vpc = Value::resource_ref("vpc", "id", vec![]);

        let interpolation = Value::Interpolation(vec![
            InterpolationPart::Literal("vpc-".to_string()),
            InterpolationPart::Expr(refs_vpc.clone()),
        ]);
        assert!(value_references_binding(&interpolation, "vpc"));

        let function_call = Value::FunctionCall {
            name: "join".to_string(),
            args: vec![Value::String(",".to_string()), refs_vpc.clone()],
        };
        assert!(value_references_binding(&function_call, "vpc"));

        let secret = Value::Secret(Box::new(refs_vpc));
        assert!(value_references_binding(&secret, "vpc"));

        // Closure variant removed from `Value` (issue #2230): closures
        // live on `EvalValue` and never reach this code path.

        // Still false when the binding is genuinely absent.
        assert!(!value_references_binding(
            &Value::String("plain".to_string()),
            "vpc"
        ));
    }

    #[test]
    fn create_row_list_of_maps_emits_pretty_attribute() {
        let mut entry = indexmap::IndexMap::new();
        entry.insert("sid".to_string(), Value::String("S1".to_string()));
        entry.insert("effect".to_string(), Value::String("Allow".to_string()));
        let resource = Resource::new("iam.RolePolicy", "test")
            .with_attribute("statement", Value::List(vec![Value::Map(entry)]));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);

        let pretty_value = rows.iter().find_map(|row| match row {
            DetailRow::PrettyAttribute { key, value } if key == "statement" => Some(value),
            _ => None,
        });
        assert!(
            pretty_value.is_some(),
            "expected PrettyAttribute row for statement, got: {rows:?}"
        );
        assert!(
            matches!(pretty_value.unwrap(), Value::List(_)),
            "PrettyAttribute should carry the raw Value::List"
        );
    }

    #[test]
    fn create_row_scalar_attribute_unchanged() {
        let resource = Resource::new("iam.Role", "test")
            .with_attribute("role_name", Value::String("foo".to_string()));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::Attribute { key, .. } if key == "role_name"
            )),
            "scalar attribute should still emit DetailRow::Attribute, got: {rows:?}"
        );
    }

    #[test]
    fn create_row_list_of_strings_emits_pretty_attribute() {
        // list-of-string must route through PrettyAttribute so
        // format_value_pretty's 80-col threshold can apply.
        let resource = Resource::new("iam.Role", "test").with_attribute(
            "managed_policy_arns",
            Value::List(vec![
                Value::String("arn:aws:iam::aws:policy/Policy1".to_string()),
                Value::String("arn:aws:iam::aws:policy/Policy2".to_string()),
            ]),
        );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);

        let pretty = rows.iter().find_map(|row| match row {
            DetailRow::PrettyAttribute { key, value } if key == "managed_policy_arns" => {
                Some(value)
            }
            _ => None,
        });
        assert!(
            pretty.is_some(),
            "expected PrettyAttribute row for list-of-string attribute, got: {rows:?}"
        );
        assert!(
            matches!(pretty.unwrap(), Value::List(_)),
            "PrettyAttribute should carry the raw Value::List"
        );
    }

    #[test]
    fn create_row_empty_list_emits_pretty_attribute() {
        // Pins down `tags = []` behavior — a regression that re-introduces
        // an `!items.is_empty()` guard would silently bypass the routing
        // for empty lists, breaking the formatting-path uniformity.
        let resource =
            Resource::new("iam.Role", "test").with_attribute("tags", Value::List(vec![]));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None);
        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::PrettyAttribute { key, .. } if key == "tags"
            )),
            "empty list should also emit PrettyAttribute, got: {rows:?}"
        );
    }
}
