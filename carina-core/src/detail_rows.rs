//! Output-neutral intermediate representation for plan detail rows.
//!
//! Both CLI and TUI consume `DetailRow` values to render effect details.
//! The `build_detail_rows` function encapsulates all logic for deciding
//! which rows to show for a given effect, keeping rendering frontends thin.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::diff_helpers::{compute_map_diff, compute_unchanged_count, schema_aware_equal};
use crate::effect::Effect;
use crate::non_empty::NonEmptyVec;
use crate::resource::{ConcreteValue, DeferredValue, ResourceId, Value};
use crate::schema::{AttributeType, ResourceSchema, SchemaRegistry, empty_defs_for_schema_walks};
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
    /// A map attribute with key-level diffs (for Update effects).
    /// `entries` is `NonEmptyVec`: a Map with no displayable entries
    /// is dropped at the IR builder (#2910), not rendered as an empty
    /// header.
    MapDiff {
        key: String,
        entries: NonEmptyVec<MapDiffEntryIR>,
    },
    /// A list-of-maps diff (for Update effects)
    ListOfMapsDiff {
        key: String,
        unchanged: Vec<String>,
        modified: Vec<ListOfMapsDiffModified>,
        added: Vec<ListOfMapsDiffItem>,
        removed: Vec<ListOfMapsDiffItem>,
    },
    /// Per-element `List<String>` diff for Update effects (#2943).
    StringListDiff {
        key: String,
        unchanged: Vec<String>,
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
        added: Vec<ListOfMapsDiffItem>,
        removed: Vec<ListOfMapsDiffItem>,
    },
    /// A map diff that forces replacement
    ReplaceMapDiff {
        key: String,
        entries: Vec<MapDiffEntryIR>,
    },
    /// Per-element `List<String>` diff that forces replacement (#2943).
    ReplaceStringListDiff {
        key: String,
        unchanged: Vec<String>,
        added: Vec<String>,
        removed: Vec<String>,
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

/// A single entry in an expanded map (e.g., tags with default_tags annotation).
///
/// `value` is kept as a raw `Value` so renderers can apply
/// `format_value_pretty` with the actual indent column. Pre-stringifying
/// at build time would force nested complex values (list-of-maps,
/// long string lists) onto a single line because a `String` carries no
/// indentation context. Renderers should pass the column at which the
/// entry's `key` is rendered as `PrettyLayout::parent_indent_cols`.
#[derive(Debug, Clone, PartialEq)]
pub struct MapExpandedEntry {
    pub key: String,
    pub value: Value,
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
    /// `entries` is `NonEmptyVec` so renderers do not need to defend
    /// against an empty container header (#2910).
    NestedMapDiff {
        key: String,
        entries: NonEmptyVec<MapDiffEntryIR>,
    },
    /// Nested list-of-maps diff: when both old and new values are lists of maps,
    /// show per-item field-level diffs instead of one-liner. `block`'s
    /// constructor enforces "at least one of unchanged/modified/added/removed
    /// is non-empty" (#2910).
    NestedListOfMapsDiff {
        key: String,
        block: NonEmptyListOfMapsBlock,
    },
    /// Per-element `List<String>` field diff inside a nested map (#3234).
    /// Mirrors `ListOfMapsDiffField::StringListChanged` but lives at the
    /// map-entry layer so a list-valued struct field (e.g.
    /// `principal.aws`) renders as multi-line `+` / `-` markers instead
    /// of the inline `[A] → [A, B]` form that overflows the terminal.
    StringListChanged {
        key: String,
        unchanged: Vec<String>,
        added: Vec<String>,
        removed: Vec<String>,
    },
}

/// A list-of-maps diff bundle whose constructor refuses the
/// "everything empty" shape (#2910). The IR builder converts via
/// [`NonEmptyListOfMapsBlock::from_parts`] and only pushes the parent
/// row when conversion succeeds, so renderers never see a section
/// header with nothing under it.
#[derive(Debug, Clone, PartialEq)]
pub struct NonEmptyListOfMapsBlock {
    unchanged: Vec<String>,
    modified: Vec<ListOfMapsDiffModified>,
    added: Vec<ListOfMapsDiffItem>,
    removed: Vec<ListOfMapsDiffItem>,
}

impl NonEmptyListOfMapsBlock {
    pub fn from_parts(
        unchanged: Vec<String>,
        modified: Vec<ListOfMapsDiffModified>,
        added: Vec<ListOfMapsDiffItem>,
        removed: Vec<ListOfMapsDiffItem>,
    ) -> Option<Self> {
        if unchanged.is_empty() && modified.is_empty() && added.is_empty() && removed.is_empty() {
            None
        } else {
            Some(Self {
                unchanged,
                modified,
                added,
                removed,
            })
        }
    }

    pub fn unchanged(&self) -> &[String] {
        &self.unchanged
    }
    pub fn modified(&self) -> &[ListOfMapsDiffModified] {
        &self.modified
    }
    pub fn added(&self) -> &[ListOfMapsDiffItem] {
        &self.added
    }
    pub fn removed(&self) -> &[ListOfMapsDiffItem] {
        &self.removed
    }
}

/// A wholly added or removed item in a list-of-maps diff (#2877).
///
/// Fields carry the raw `Value` rather than a pre-stringified form so the
/// renderer can supply the actual indent column when calling
/// `format_value_pretty`. Pre-stringifying at build time would force
/// nested complex values (long string lists, nested maps) onto a single
/// line because a `String` carries no indentation context — that was
/// exactly the bug fixed in #2877.
#[derive(Debug, Clone, PartialEq)]
pub struct ListOfMapsDiffItem {
    /// Map fields in the order they should be rendered (alphabetical).
    pub fields: Vec<(String, Value)>,
}

/// Whether a `ListOfMapsDiffItem` is a wholly-added or wholly-removed
/// element. Lives in `carina-core` so both the CLI and TUI renderers can
/// share the discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOfMapsDiffItemKind {
    Added,
    Removed,
}

/// A modified item in a list-of-maps diff.
///
/// `fields` is `NonEmptyVec`: an item with zero changed fields is by
/// definition unchanged and the IR builder drops it (#2886) rather
/// than synthesize a `~ {}` row. Renderers therefore do not need to
/// guard against an empty `fields` shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ListOfMapsDiffModified {
    pub fields: NonEmptyVec<ListOfMapsDiffField>,
    /// Number of unchanged fields filtered out of `fields`. Set to a
    /// non-zero value only when the build is in `DetailLevel::Full`,
    /// matching the top-level `# (n unchanged attributes hidden)`
    /// convention. Zero in `Explicit` / `NamesOnly`. Renderers should
    /// emit a `# (n unchanged fields hidden)` line when this is > 0.
    pub hidden_unchanged_count: usize,
}

/// A single field in a modified list-of-maps item
#[derive(Debug, Clone, PartialEq)]
pub enum ListOfMapsDiffField {
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
    /// Per-element `List<String>` field diff inside a modified
    /// list-of-maps element (#2943).
    StringListChanged {
        key: String,
        unchanged: Vec<String>,
        added: Vec<String>,
        removed: Vec<String>,
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

/// Resolve the subtype for map entry `key`: a `Map`'s `value` type, a
/// `Struct` field's type, or `None`. `List`/`Union` are unwrapped so a
/// nested `List<Map>`/`List<Struct>` still resolves its entries
/// (carina#3073). Uses the canonical `build_accepted_field_map` so a
/// `block_name`-aliased struct field resolves like `validate_struct`.
///
/// `defs` is the enclosing schema's `defs` map; any [`AttributeType::Ref`]
/// reached during unwrap is peeled against it. Without this, a
/// `Ref`-typed attribute (cyclic CFN: `lifecycle_configuration:
/// Ref("LifecycleConfiguration")`) returns `None` for every entry —
/// the plan-display detail rows lose schema-aware classification and
/// the `# n unchanged attributes hidden` tally drifts (same bug class
/// as carina#3349; carina#3340 walk-site doc-comment names
/// `detail_rows` as a Ref-aware walker).
fn map_entry_subtype<'a>(
    attr_type: Option<&'a AttributeType>,
    key: &str,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let mut t = attr_type?;
    loop {
        // Project onto `Shape` so any `Ref` chain is peeled at the
        // type level (carina#3349). The wildcard arm cannot
        // silently drop a `Ref` because `Shape` has no `Ref`
        // variant. `shape(defs)` panics on a dangling `Ref` —
        // schema-construction bug, surfaced loudly.
        match t.shape_with_defs(defs) {
            crate::schema::Shape::List {
                element_type: inner,
                ..
            } => t = inner,
            crate::schema::Shape::Map { value, .. } => return Some(value),
            crate::schema::Shape::Struct { .. } => {
                let fields = crate::schema::struct_fields_with_defs(t, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                // Canonical field accessor — resolves `block_name`
                // aliases too, matching `validate_struct` /
                // `collect_struct` (#2214).
                return crate::schema::build_accepted_field_map(fields)
                    .get(key)
                    .map(|f| &f.field_type);
            }
            crate::schema::Shape::Union => {
                let members = crate::schema::union_members_with_defs(t, defs)
                    .expect("Shape::Union must expose union members internally");
                t = members.iter().find(|m| {
                    matches!(
                        &m.kind,
                        crate::schema::AttrTypeKind::Struct { .. }
                            | crate::schema::AttrTypeKind::Map { .. }
                            | crate::schema::AttrTypeKind::List { .. }
                    )
                })?;
            }
            _ => return None,
        }
    }
}

/// Build detail rows for an effect.
///
/// This function encapsulates ALL the logic for deciding what detail rows to
/// show for a given effect. The caller only needs to render each `DetailRow`
/// with appropriate formatting (colors, prefixes, etc.).
///
/// `prev_explicit` is the per-resource user-authoring tree the differ used.
/// When provided, the actual-state side (`from.attributes`) is projected
/// through it before unchanged-attribute counting so server-side default
/// fields the user never wrote do not inflate the
/// `# (n unchanged attributes hidden)` summary in Full mode (refs awscc#206).
pub fn build_detail_rows(
    effect: &Effect,
    registry: Option<&SchemaRegistry>,
    detail: DetailLevel,
    delete_attributes: Option<&HashMap<ResourceId, HashMap<String, Value>>>,
    prev_explicit: Option<&HashMap<ResourceId, crate::explicit::ExplicitFields>>,
) -> Vec<DetailRow> {
    if detail == DetailLevel::NamesOnly {
        return Vec::new();
    }

    // carina#3181: `Effect` payloads are typestate structs — the
    // managed variants carry `Resource`, `Read` carries a
    // `DataSource`. Schema lookup routes through the matching
    // `get_for` / `get_for_data_source` registry method.
    match effect {
        Effect::Create(r) => {
            let schema = registry.and_then(|reg| reg.get_for(r));
            build_create_rows(&r.attributes, schema, detail)
        }
        Effect::Update {
            from,
            to,
            changed_attributes,
            ..
        } => {
            let explicit = prev_explicit.and_then(|map| map.get(&to.id));
            let schema = registry.and_then(|r| r.get_for(to));
            build_update_rows(from, to, changed_attributes, schema, detail, explicit)
        }
        Effect::Replace {
            from,
            to,
            changed_create_only,
            cascading_updates,
            temporary_name,
            cascade_ref_hints,
            ..
        } => {
            let explicit = prev_explicit.and_then(|map| map.get(&to.id));
            let schema = registry.and_then(|r| r.get_for(to));
            build_replace_rows(
                from,
                to,
                changed_create_only,
                cascading_updates,
                temporary_name,
                cascade_ref_hints,
                schema,
                detail,
                explicit,
            )
        }
        Effect::Delete { id, .. } => build_delete_rows(id, delete_attributes),
        Effect::Read { resource } => {
            let schema = registry.and_then(|reg| reg.get_for_data_source(resource));
            build_create_rows(&resource.attributes, schema, detail)
        }
        Effect::Import { identifier, .. } => {
            // carina#3329: the identifier is carried as a `Value` so a
            // deferred upstream-state reference inside a `"${X.attr}|…"`
            // interpolation renders as `(known after upstream apply: …)`
            // instead of being silently substituted to empty.
            // `format_import_identifier` prints concrete identifiers bare
            // and falls back to the structured `format_value_with_key`
            // shape only for the deferred / interpolation case.
            vec![DetailRow::Attribute {
                key: "id".to_string(),
                value: crate::effect::format_import_identifier(identifier),
                ref_binding: None,
                annotation: None,
            }]
        }
        Effect::Remove { .. } | Effect::Move { .. } | Effect::Wait { .. } => Vec::new(),
    }
}

fn build_create_rows(
    attributes: &IndexMap<String, Value>,
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    // Collect default_tag_keys for annotation
    let default_tag_keys: HashSet<String> = attributes
        .get("_default_tag_keys")
        .and_then(|v| match v {
            Value::Concrete(ConcreteValue::List(items)) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    let mut keys: Vec<_> = attributes.keys().filter(|k| !k.starts_with('_')).collect();
    keys.sort();

    for key in &keys {
        let value = &attributes[*key];
        // Expand tags map into individual rows with default_tags annotation
        if key.as_str() == "tags"
            && !default_tag_keys.is_empty()
            && let Value::Concrete(ConcreteValue::Map(map)) = value
        {
            rows.push(build_expanded_tags_row(map, &default_tag_keys));
            continue;
        }
        // `Value::Concrete(ConcreteValue::List)` (any element type) goes through PrettyAttribute so
        // `format_value_pretty` can apply its 80-col threshold and YAML-style
        // vertical layout. `Value::Concrete(ConcreteValue::Map)` keeps the existing MapExpanded path
        // because that variant carries per-entry annotation slots that
        // PrettyAttribute does not represent (used by tags/default_tags).
        if let Value::Concrete(ConcreteValue::List(_)) = value {
            rows.push(DetailRow::PrettyAttribute {
                key: key.to_string(),
                value: value.clone(),
            });
        } else if let Value::Concrete(ConcreteValue::Map(map)) = value {
            rows.push(build_expanded_map_row(key, map));
        } else {
            let ref_binding = match value {
                Value::Deferred(DeferredValue::ResourceRef { path }) => {
                    Some(path.binding().to_string())
                }
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
        && let Some(schema) = schema
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
            value: map[k].clone(),
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
                value: value.clone(),
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
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
    explicit: Option<&crate::explicit::ExplicitFields>,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    let defs = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());

    // Project `from.attributes` through the user-authoring tree so
    // server-side default fields the user never wrote don't inflate
    // the unchanged-count summary (refs awscc#206). Idempotent and a
    // no-op when `explicit` is None.
    let from_attrs_projected: HashMap<String, Value> = match explicit {
        Some(e) => crate::explicit::project_attributes(from.attributes.clone(), e),
        None => from.attributes.clone(),
    };

    let mut keys: Vec<_> = to
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    // Attributes where `semantically_equal` reported a diff but the
    // per-shape builder produced no display rows. Two shapes today:
    // list-of-maps whose only diff was an upstream-injected key the IR
    // dropped (#2886), and Map whose only entries were empty nested
    // sections recursively suppressed (#2910). Counted into the
    // trailing unchanged-attributes summary so the final tally still
    // adds up.
    let mut effectively_unchanged: usize = 0;

    for key in keys {
        let new_value = &to.attributes[key];
        // Look up `old_value` from the projected map so MapDiff /
        // ListOfMapsDiff rows don't surface server-side default leaves
        // the user never authored (refs awscc#206). The same value the
        // unchanged-count computation below uses, kept consistent so a
        // field that's "unchanged after projection" is also "unchanged
        // for display".
        let old_value = from_attrs_projected.get(key);
        let attr_type = schema
            .and_then(|s| s.attributes.get(key.as_str()))
            .map(|a| &a.attr_type);
        // Site 1 (carina#3073): schema-aware top-level equality. When
        // the only diffs inside this attribute are enum-equal leaves
        // (the IAM-policy phantom), `type_aware_equal` recurses
        // Struct/List/Map internally and returns true, so no row is
        // built at all and sites 3/4/5 never run for this attribute.
        let is_same = old_value
            .map(|ov| schema_aware_equal(ov, new_value, attr_type, defs))
            .unwrap_or(false);

        if is_same {
            continue;
        }

        if is_list_of_maps(new_value) {
            match build_list_of_maps_diff_row(key, old_value, new_value, attr_type, defs, detail) {
                Some(row) => rows.push(row),
                None => effectively_unchanged += 1,
            }
        } else if should_render_as_map_diff(old_value, new_value) {
            match build_map_diff_row(key, old_value, new_value, attr_type, defs, detail) {
                Some(row) => rows.push(row),
                None => effectively_unchanged += 1,
            }
        } else if let Some(diff) = compute_string_list_change(old_value, new_value, attr_type) {
            rows.push(DetailRow::StringListDiff {
                key: key.to_string(),
                unchanged: diff.unchanged,
                added: diff.added,
                removed: diff.removed,
            });
        } else {
            let old_str = old_value
                .map(|v| format_value_with_key(v, Some(key)))
                .unwrap_or_else(|| "(none)".to_string());
            let new_str = format_value_with_key(new_value, Some(key));
            // carina#3258: if the attribute's two sides render
            // identically, `~ key: X → X` is a lie — the upstream
            // differ flagged the attribute as changed but the display
            // layer collapses both sides to the same string (commonly
            // a value-shape mismatch like `StringList` vs
            // `List<String>` that the renderer cannot draw). Fold
            // into the hidden-unchanged tally instead. Sibling guards
            // live in `compute_map_diff_entries` and
            // `compute_list_of_maps_diff_parts` below.
            //
            // Skip the guard when either side contains a secret:
            // `format_value` collapses every secret to the literal
            // `"(secret)"`, so display equality is meaningless and
            // suppression would hide a real secret rotation
            // (e.g. `Secret(hash_A) → Secret(hash_B)`) — pre-fix the
            // user saw an uninformative `~ … (secret) → (secret)` row;
            // suppressing it would silently hide the rotation.
            if old_str == new_str
                && old_value.is_none_or(|v| !crate::value::contains_secret(v))
                && !crate::value::contains_secret(new_value)
            {
                effectively_unchanged += 1;
                continue;
            }
            rows.push(DetailRow::Changed {
                key: key.to_string(),
                old: old_str,
                new: new_str,
            });
        }
    }

    // Show removed attributes (in changed_attributes but not in to).
    // Use the projected map so we don't surface unauthored leaves as
    // removals (refs awscc#206).
    let mut removed_keys: Vec<_> = changed_attributes
        .iter()
        .filter(|k| !to.attributes.contains_key(k.as_str()))
        .collect();
    removed_keys.sort();
    for key in removed_keys {
        if let Some(old_value) = from_attrs_projected.get(key.as_str()) {
            // Mirror the addition direction (#2936): when a Map
            // attribute is dropped entirely, route through `MapDiff`
            // so each key renders on its own line as `- key: "value"`
            // instead of overflowing on a single inline `{...} → (removed)`
            // row (#2939).
            if let Some(row) = build_map_removed_row(key, old_value) {
                rows.push(row);
                continue;
            }
            rows.push(DetailRow::Removed {
                key: key.to_string(),
                old: format_value_with_key(old_value, Some(key)),
            });
        }
    }

    // In Full mode, show count of unchanged attributes hidden
    if detail == DetailLevel::Full {
        let unchanged_count = compute_unchanged_count(
            &from_attrs_projected,
            &to.resolved_attributes(),
            None,
            schema,
        ) + effectively_unchanged;
        if unchanged_count > 0 {
            rows.push(DetailRow::HiddenUnchanged {
                count: unchanged_count,
            });
        }
    }

    rows
}

#[allow(clippy::too_many_arguments)]
fn build_replace_rows(
    from: &crate::resource::State,
    to: &crate::resource::Resource,
    changed_create_only: &[String],
    cascading_updates: &[crate::effect::CascadingUpdate],
    temporary_name: &Option<crate::effect::TemporaryName>,
    cascade_ref_hints: &[(String, String)],
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
    explicit: Option<&crate::explicit::ExplicitFields>,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    let defs = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());

    // Show changed create-only attributes
    let mut keys: Vec<_> = changed_create_only
        .iter()
        .filter(|k| to.attributes.contains_key(k.as_str()))
        .collect();
    keys.sort();

    for key in keys {
        let new_value = &to.attributes[key.as_str()];
        let old_value = from.attributes.get(key.as_str());
        let attr_type = schema
            .and_then(|s| s.attributes.get(key.as_str()))
            .map(|a| &a.attr_type);
        // Site 2 (carina#3073): schema-aware, mirrors site 1.
        let is_same = old_value
            .map(|ov| schema_aware_equal(ov, new_value, attr_type, defs))
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
                compute_list_of_maps_diff_parts(old_value, new_value, attr_type, defs, detail);
            rows.push(DetailRow::ReplaceListOfMapsDiff {
                key: key.to_string(),
                unchanged,
                modified,
                added,
                removed,
            });
        } else if should_render_as_map_diff(old_value, new_value) {
            let entries = compute_map_diff_entries(old_value, new_value, attr_type, defs, detail);
            rows.push(DetailRow::ReplaceMapDiff {
                key: key.to_string(),
                entries,
            });
        } else if let Some(diff) = compute_string_list_change(old_value, new_value, attr_type) {
            rows.push(DetailRow::ReplaceStringListDiff {
                key: key.to_string(),
                unchanged: diff.unchanged,
                added: diff.added,
                removed: diff.removed,
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

    // In Full mode, show count of unchanged attributes hidden.
    // Project `from.attributes` so server-side defaults don't inflate
    // the count (refs awscc#206).
    if detail == DetailLevel::Full {
        let from_attrs_projected: HashMap<String, Value> = match explicit {
            Some(e) => crate::explicit::project_attributes(from.attributes.clone(), e),
            None => from.attributes.clone(),
        };
        let changed_set: HashSet<&str> = changed_create_only.iter().map(|s| s.as_str()).collect();
        let unchanged_count = compute_unchanged_count(
            &from_attrs_projected,
            &to.resolved_attributes(),
            Some(&changed_set),
            schema,
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
            // Route lists through PrettyAttribute so `format_value_pretty`
            // applies its 80-col threshold and YAML-style vertical layout,
            // mirroring `build_create_rows`. Without this, a list-of-maps
            // like `domain_validation_options` renders on one long,
            // unreadable line (the Delete path was asymmetric with Create).
            if let Value::Concrete(ConcreteValue::List(_)) = value {
                rows.push(DetailRow::PrettyAttribute {
                    key: key.to_string(),
                    value: value.clone(),
                });
            } else if let Value::Concrete(ConcreteValue::Map(map)) = value {
                rows.push(build_expanded_map_row(key, map));
            } else {
                let ref_binding = match value {
                    Value::Deferred(DeferredValue::ResourceRef { path }) => {
                        Some(path.binding().to_string())
                    }
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

/// Build a `DetailRow::MapDiff` whose entries are all `Removed`,
/// mirroring the addition-direction path that emits all-`Added`
/// entries when an attribute appears for the first time (#2936).
///
/// Returns `None` when `old_value` is not a `Map`, or when the map
/// is empty (single inline `tags: {} → (removed)` rendering is fine
/// at that size). The caller falls through to the inline
/// `DetailRow::Removed` form in both cases. Used by `build_update_rows`
/// to fix #2939, where a multi-key map dropped from a resource used
/// to render inline on one overflowing line.
fn build_map_removed_row(key: &str, old_value: &Value) -> Option<DetailRow> {
    let Value::Concrete(ConcreteValue::Map(old_map)) = old_value else {
        return None;
    };
    if old_map.is_empty() {
        return None;
    }
    let mut sorted_keys: Vec<&String> = old_map.keys().collect();
    sorted_keys.sort();
    let entries: Vec<MapDiffEntryIR> = sorted_keys
        .into_iter()
        .map(|k| MapDiffEntryIR::Removed {
            key: k.clone(),
            value: format_value_with_key(&old_map[k], Some(k)),
        })
        .collect();
    let entries = NonEmptyVec::from_vec(entries)?;
    Some(DetailRow::MapDiff {
        key: key.to_string(),
        entries,
    })
}

/// Returns `None` when `compute_map_diff_entries` filters every entry
/// (#2910), so the caller can fold the attribute into the trailing
/// `# (n unchanged attributes hidden)` count — same shape as
/// `build_list_of_maps_diff_row` (#2886).
fn build_map_diff_row(
    key: &str,
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
) -> Option<DetailRow> {
    let entries = NonEmptyVec::from_vec(compute_map_diff_entries(
        old_value, new_value, attr_type, defs, detail,
    ))?;
    Some(DetailRow::MapDiff {
        key: key.to_string(),
        entries,
    })
}

fn compute_map_diff_entries(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
) -> Vec<MapDiffEntryIR> {
    let new_map = match new_value {
        Value::Concrete(ConcreteValue::Map(m)) => m,
        _ => return Vec::new(),
    };
    let old_map = match old_value {
        Some(Value::Concrete(ConcreteValue::Map(m))) => m,
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
                let entry_type = map_entry_subtype(attr_type, &e.key, defs);
                // Site 5 (carina#3073): `compute_map_diff` flagged this
                // entry as changed via schema-blind `semantically_equal`.
                // For a scalar/leaf entry, re-test schema-aware: an
                // enum-equal leaf (`EnumIdentifier("allow")` vs
                // `String("Allow")`) is not a real change — skip its
                // phantom row. Containers (nested Map / list-of-maps)
                // are not filtered here; they recurse and self-filter
                // below. (No `entry_type.is_some()` guard: with no
                // type, `schema_aware_equal` is `semantically_equal`,
                // which `compute_map_diff` already used to classify
                // this entry as changed, so the branch is never taken.)
                let is_recursed_container =
                    matches!(&e.new_value, Value::Concrete(ConcreteValue::Map(_)))
                        || is_list_of_maps(&e.new_value);
                if !is_recursed_container
                    && schema_aware_equal(&e.old_value, &e.new_value, entry_type, defs)
                {
                    continue;
                }
                // If both old and new are maps, recursively diff
                if matches!(&e.old_value, Value::Concrete(ConcreteValue::Map(_)))
                    && matches!(&e.new_value, Value::Concrete(ConcreteValue::Map(_)))
                {
                    let nested = compute_map_diff_entries(
                        Some(&e.old_value),
                        &e.new_value,
                        entry_type,
                        defs,
                        detail,
                    );
                    // #2910: `from_vec` returns None for the all-empty
                    // recursive case (every grandchild was itself
                    // suppressed). Skip pushing so the parent header
                    // never renders empty.
                    if let Some(entries_nev) = NonEmptyVec::from_vec(nested) {
                        entries.push(MapDiffEntryIR::NestedMapDiff {
                            key: e.key.clone(),
                            entries: entries_nev,
                        });
                    }
                } else if is_list_of_maps(&e.new_value) {
                    // List-of-maps: compute per-item field-level diffs
                    let (_, modified, added, removed) = compute_list_of_maps_diff_parts(
                        Some(&e.old_value),
                        &e.new_value,
                        entry_type,
                        defs,
                        detail,
                    );
                    // #2910: `from_parts` returns None when every
                    // paired element was dropped per #2886 and there
                    // are no wholly-added / wholly-removed elements
                    // either.
                    if let Some(block) =
                        NonEmptyListOfMapsBlock::from_parts(Vec::new(), modified, added, removed)
                    {
                        entries.push(MapDiffEntryIR::NestedListOfMapsDiff {
                            key: e.key.clone(),
                            block,
                        });
                    }
                } else if let Some(diff) =
                    compute_string_list_change(Some(&e.old_value), &e.new_value, entry_type)
                {
                    // #3234: see MapDiffEntryIR::StringListChanged docs.
                    entries.push(MapDiffEntryIR::StringListChanged {
                        key: e.key.clone(),
                        unchanged: diff.unchanged,
                        added: diff.added,
                        removed: diff.removed,
                    });
                } else {
                    let old_s = format_value_with_key(&e.old_value, Some(&e.key));
                    let new_s = format_value_with_key(&e.new_value, Some(&e.key));
                    // carina#3258: see the renderer-truth note in
                    // `build_update_rows` (including the secret
                    // bypass). Map-level hidden counts roll up at the
                    // attribute level, so silently dropping the entry
                    // (matching the schema-aware leaf-skip arm above)
                    // is the correct behavior here.
                    if old_s == new_s
                        && !crate::value::contains_secret(&e.old_value)
                        && !crate::value::contains_secret(&e.new_value)
                    {
                        continue;
                    }
                    entries.push(MapDiffEntryIR::Changed {
                        key: e.key.clone(),
                        old: old_s,
                        new: new_s,
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

/// Build the per-attribute list-of-maps diff row, returning `None` when
/// every section of the diff is empty. The empty result occurs when the
/// only "difference" was an upstream-injected key that the IR builder
/// dropped (#2886) — semantically the attribute has no diff to show, so
/// the caller should treat it as effectively unchanged and let it roll
/// into the top-level `# (n unchanged attributes hidden)` summary.
fn build_list_of_maps_diff_row(
    key: &str,
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
) -> Option<DetailRow> {
    let (unchanged, modified, added, removed) =
        compute_list_of_maps_diff_parts(old_value, new_value, attr_type, defs, detail);
    if unchanged.is_empty() && modified.is_empty() && added.is_empty() && removed.is_empty() {
        return None;
    }
    Some(DetailRow::ListOfMapsDiff {
        key: key.to_string(),
        unchanged,
        modified,
        added,
        removed,
    })
}

fn compute_list_of_maps_diff_parts(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
) -> (
    Vec<String>,
    Vec<ListOfMapsDiffModified>,
    Vec<ListOfMapsDiffItem>,
    Vec<ListOfMapsDiffItem>,
) {
    let new_items = match new_value {
        Value::Concrete(ConcreteValue::List(items)) => items,
        _ => return (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
    };
    let old_items = match old_value {
        Some(Value::Concrete(ConcreteValue::List(items))) => items,
        _ => &vec![] as &Vec<Value>,
    };

    // The element type for the list (e.g. the IAM policy `statement`
    // `Struct`); used for schema-aware item/field equality below.
    // Peel any leading `Ref` so a `Ref("…")` attribute whose def is
    // `List<Struct>` still drops into the List arm — same bug class
    // as carina#3349. `resolve_refs` is a no-op on non-Ref inputs.
    let attr_type_peeled = attr_type.map(|t| t.resolve_refs_with_defs(defs).as_attr());
    let item_type = match attr_type_peeled.map(|t| (&t.kind, t)) {
        Some((
            crate::schema::AttrTypeKind::List {
                element_type: inner,
                ..
            },
            _,
        )) => Some(inner.as_ref()),
        // The attribute itself may already be the element type when
        // this is reached recursively from a Map value.
        _ => attr_type_peeled,
    };

    let mut old_matched = vec![false; old_items.len()];
    let mut new_matched = vec![false; new_items.len()];

    // Phase 1: Find exact matches. Site 3 (carina#3073): schema-aware
    // so two statements differing only by enum spelling
    // (`effect: allow` vs `Allow`) match as identical instead of
    // being reported as removed+added.
    for (ni, new_item) in new_items.iter().enumerate() {
        for (oi, old_item) in old_items.iter().enumerate() {
            if !old_matched[oi] && schema_aware_equal(old_item, new_item, item_type, defs) {
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
        if let Value::Concrete(ConcreteValue::Map(map)) = new_item
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
        if let (
            Value::Concrete(ConcreteValue::Map(old_map)),
            Value::Concrete(ConcreteValue::Map(new_map)),
        ) = (&old_items[oi], &new_items[ni])
        {
            let mut keys: Vec<_> = new_map.keys().collect();
            keys.sort();
            let mut fields: Vec<ListOfMapsDiffField> = Vec::new();
            let mut unchanged_count: usize = 0;
            for k in keys {
                // Site 4 (carina#3073): resolve the struct field's
                // type so an enum-equal field (`effect: allow` vs
                // `Allow`) is treated as unchanged instead of emitting
                // a phantom `~ effect` sub-row under a sibling-changed
                // statement.
                let field_type = map_entry_subtype(item_type, k, defs);
                let field_same = old_map
                    .get(k)
                    .map(|ov| schema_aware_equal(ov, &new_map[k], field_type, defs))
                    .unwrap_or(false);
                if field_same {
                    // #2881: drop unchanged fields from the IR; renderers
                    // surface them as `# (n unchanged fields hidden)`,
                    // mirroring the top-level `HiddenUnchanged` row.
                    unchanged_count += 1;
                    continue;
                }
                let old_val = old_map.get(k);
                if matches!(old_val, Some(Value::Concrete(ConcreteValue::Map(_))))
                    && matches!(&new_map[k], Value::Concrete(ConcreteValue::Map(_)))
                {
                    let nested =
                        compute_map_diff_entries(old_val, &new_map[k], field_type, defs, detail);
                    // carina#3258: when every nested entry was
                    // suppressed (display-equal phantom or
                    // schema-aware leaf-skip), the parent header alone
                    // — `~ { condition: }` — is itself a phantom row.
                    // Fold the field into the hidden-unchanged count
                    // instead of pushing an empty `NestedMapChanged`.
                    // Mirrors the same guard at the sibling
                    // `NestedMapDiff` site in
                    // `compute_map_diff_entries` (#2910).
                    if nested.is_empty() {
                        unchanged_count += 1;
                        continue;
                    }
                    fields.push(ListOfMapsDiffField::NestedMapChanged {
                        key: k.to_string(),
                        entries: nested,
                    });
                } else if let Some(diff) =
                    compute_string_list_change(old_val, &new_map[k], field_type)
                {
                    fields.push(ListOfMapsDiffField::StringListChanged {
                        key: k.to_string(),
                        unchanged: diff.unchanged,
                        added: diff.added,
                        removed: diff.removed,
                    });
                } else {
                    let old_v = old_val
                        .map(format_value)
                        .unwrap_or_else(|| "(none)".to_string());
                    let new_v = format_value(&new_map[k]);
                    // carina#3258: see the renderer-truth note in
                    // `build_update_rows` (including the secret
                    // bypass). Fold display-equal peers into the
                    // per-element hidden-unchanged count so the
                    // summary stays consistent with the rendered
                    // evidence.
                    if old_v == new_v
                        && old_val.is_none_or(|v| !crate::value::contains_secret(v))
                        && !crate::value::contains_secret(&new_map[k])
                    {
                        unchanged_count += 1;
                        continue;
                    }
                    fields.push(ListOfMapsDiffField::Changed {
                        key: k.to_string(),
                        old: old_v,
                        new: new_v,
                    });
                }
            }
            // #2886: a paired item whose every desired key matched
            // state (`fields` empty) is semantically unchanged. Drop
            // it from the modified list rather than synthesizing a
            // `~ {}` row that would mislead the reader. The driver of
            // this case is upstream provider drift — state carrying a
            // key not in desired forces Phase 1 to reject the pair on
            // length mismatch, but Phase 2 still pairs by similarity.
            let Some(fields) = NonEmptyVec::from_vec(fields) else {
                continue;
            };
            // Only surface the count in Full mode — Explicit / NamesOnly
            // never emit hidden-counts at the top level either.
            let hidden_unchanged_count = if detail == DetailLevel::Full {
                unchanged_count
            } else {
                0
            };
            modified.push(ListOfMapsDiffModified {
                fields,
                hidden_unchanged_count,
            });
        }
    }

    let added = collect_added_removed_items(&added_indices, new_items);
    let removed = collect_added_removed_items(&removed_indices, old_items);

    (unchanged, modified, added, removed)
}

/// Format a `# (n unchanged <noun> hidden)` summary for use under both
/// the top-level `DetailRow::HiddenUnchanged` row (noun = "attribute")
/// and the per-element list-of-maps unchanged-fields summary
/// (noun = "field"). Pluralizes in English (singular / `<noun>s`).
/// Defined here so CLI and TUI renderers share one source of truth and
/// don't drift in punctuation or wording.
///
/// ```
/// use carina_core::detail_rows::hidden_unchanged_summary;
/// assert_eq!(
///     hidden_unchanged_summary(1, "attribute"),
///     "# (1 unchanged attribute hidden)"
/// );
/// assert_eq!(
///     hidden_unchanged_summary(3, "field"),
///     "# (3 unchanged fields hidden)"
/// );
/// ```
pub fn hidden_unchanged_summary(count: usize, noun_singular: &str) -> String {
    let noun = if count == 1 {
        noun_singular.to_string()
    } else {
        format!("{}s", noun_singular)
    };
    format!("# ({} unchanged {} hidden)", count, noun)
}

/// Build alphabetically-sorted `ListOfMapsDiffItem`s from a list of map
/// indices into `items`. Non-map entries at the listed indices are silently
/// skipped — only `Value::Concrete(ConcreteValue::Map)` items contribute fields. This is what the
/// renderer consumes for the added/removed slots of a list-of-maps diff.
fn collect_added_removed_items(indices: &[usize], items: &[Value]) -> Vec<ListOfMapsDiffItem> {
    indices
        .iter()
        .filter_map(|&i| {
            if let Value::Concrete(ConcreteValue::Map(map)) = &items[i] {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                let fields = entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Some(ListOfMapsDiffItem { fields })
            } else {
                None
            }
        })
        .collect()
}

/// Check whether the diff at this attribute should render via the
/// per-key `MapDiff` walk. True when `new_value` is a `Value::Concrete(ConcreteValue::Map)` and
/// `old_value` is either absent (attribute being added — #2936) or
/// itself a `Value::Concrete(ConcreteValue::Map)`. A non-Map old value (type mismatch, e.g.
/// string → map) keeps the inline `prev → next` form so the prior
/// scalar stays visible.
fn should_render_as_map_diff(old_value: Option<&Value>, new_value: &Value) -> bool {
    matches!(new_value, Value::Concrete(ConcreteValue::Map(_)))
        && matches!(
            old_value,
            None | Some(Value::Concrete(ConcreteValue::Map(_)))
        )
}

/// Compute a string-list diff for a single attribute (#2943).
///
/// Returns `Some(diff)` when `new_value` is a string-list shape,
/// `old_value` is either absent or also a string-list shape, and the
/// diff has at least one added or removed element. Returns `None`
/// otherwise — the caller falls through to the inline `Changed` form
/// so a type mismatch (e.g. string → list) keeps the prior scalar
/// visible.
fn compute_string_list_change(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
) -> Option<crate::diff_helpers::StringListDiff> {
    let mut new_list = crate::value::as_string_list(new_value)?;
    let mut old_list = match old_value {
        Some(ov) => crate::value::as_string_list(ov)?,
        None => Vec::new(),
    };
    // carina#3075: when the element type is a `Enum`, the set-diff
    // must compare enum *values*, not raw spellings — otherwise a
    // DSL-alias element (`"allow"`) vs its API-canonical form
    // (`"Allow"`) is reported as a phantom `- allow / + Allow` pair
    // alongside any genuine add/remove. Canonicalize every element to
    // the API spelling first, mirroring the differ's `Enum` arm
    // (`extract_enum_value_with_values` → `DslMap::api_for`).
    if let Some((_, Some(values), _, _, dsl_map)) = attr_type
        .and_then(string_list_inner_type)
        .and_then(|t| t.enum_parts())
    {
        let valid: Vec<&str> = values.iter().map(String::as_str).collect();
        for s in old_list.iter_mut().chain(new_list.iter_mut()) {
            *s = crate::utils::canonicalize_enum_to_api(s, &valid, &dsl_map);
        }
    }
    let diff = crate::diff_helpers::compute_string_list_diff(&old_list, &new_list);
    if diff.added.is_empty() && diff.removed.is_empty() {
        return None;
    }
    Some(diff)
}

/// Return a `List`'s element type, descending recursively through
/// `Union` members to find the first `List` arm. Returns `None` when
/// `attr_type` resolves to nothing list-shaped.
fn string_list_inner_type(attr_type: &AttributeType) -> Option<&AttributeType> {
    use crate::schema::AttrTypeKind;
    match &attr_type.kind {
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => Some(inner),
        AttrTypeKind::Union(members) => members.iter().find_map(string_list_inner_type),
        // `AttributeType::Ref` (carina#3340): in CFN-derived schemas
        // the resolved target is a `Struct`, never a `List<String>`.
        // Returning `None` is the safe answer and keeps the helper
        // free of a `defs` dependency. A future schema that uses
        // `Ref` for list shapes would need to thread `&defs` and
        // resolve here.
        AttrTypeKind::Ref(_) => None,
        AttrTypeKind::String { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. } => None,
    }
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
    fn non_empty_list_of_maps_block_rejects_all_empty() {
        assert!(
            NonEmptyListOfMapsBlock::from_parts(Vec::new(), Vec::new(), Vec::new(), Vec::new())
                .is_none()
        );
    }

    #[test]
    fn non_empty_list_of_maps_block_accepts_any_non_empty_section() {
        let block = NonEmptyListOfMapsBlock::from_parts(
            vec!["{ k: v }".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("non-empty unchanged section is valid");
        assert_eq!(block.unchanged().len(), 1);
        assert!(block.modified().is_empty());
    }

    #[test]
    fn test_names_only_returns_empty() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::NamesOnly, None, None);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_create_basic_attributes() {
        let resource = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "bucket",
                Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
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
                Value::Concrete(ConcreteValue::String("Disabled".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket").with_attribute(
            "versioning",
            Value::Concrete(ConcreteValue::String("Enabled".to_string())),
        );
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["versioning".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Changed { key, old, new }
            if key == "versioning" && old == "\"Disabled\"" && new == "\"Enabled\""));
    }

    #[test]
    fn test_update_hidden_unchanged_in_full_mode() {
        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("test".to_string())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
                ),
                (
                    "versioning".to_string(),
                    Value::Concrete(ConcreteValue::String("Disabled".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("test".to_string())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            )
            .with_attribute(
                "versioning",
                Value::Concrete(ConcreteValue::String("Enabled".to_string())),
            );
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["versioning".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None, None);
        // 1 changed + 1 hidden unchanged (2 unchanged attrs)
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], DetailRow::Changed { .. }));
        assert!(matches!(&rows[1], DetailRow::HiddenUnchanged { count: 2 }));
    }

    #[test]
    fn unchanged_count_excludes_server_only_field() {
        // `from.attributes` has authored + server_only, both unchanged
        // against `to`. Without prev_explicit, both contribute to the
        // hidden-unchanged count. With prev_explicit listing only
        // "authored", the projection drops server_only before counting,
        // shrinking the count from 2 to 1 — the awscc#206 effect.
        use crate::explicit::ExplicitFields;

        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [
                (
                    "authored".to_string(),
                    Value::Concrete(ConcreteValue::String("a".to_string())),
                ),
                (
                    "server_only".to_string(),
                    Value::Concrete(ConcreteValue::String("s".to_string())),
                ),
                (
                    "trigger_diff".to_string(),
                    Value::Concrete(ConcreteValue::String("old-value".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "authored",
                Value::Concrete(ConcreteValue::String("a".to_string())),
            )
            .with_attribute(
                "server_only",
                Value::Concrete(ConcreteValue::String("s".to_string())),
            )
            .with_attribute(
                "trigger_diff",
                Value::Concrete(ConcreteValue::String("new-value".to_string())),
            );
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["trigger_diff".to_string()],
        };

        // Without prev_explicit: both authored and server_only contribute
        // to the unchanged count.
        let rows_no_explicit = build_detail_rows(&effect, None, DetailLevel::Full, None, None);
        let no_explicit_count = rows_no_explicit.iter().find_map(|r| match r {
            DetailRow::HiddenUnchanged { count } => Some(*count),
            _ => None,
        });
        assert_eq!(
            no_explicit_count,
            Some(2),
            "Without prev_explicit, both authored and server_only contribute to unchanged count"
        );

        // With prev_explicit listing only "authored": server_only is
        // projected out, leaving only "authored" as the unchanged tally.
        let mut explicit_map = HashMap::new();
        explicit_map.insert(
            ResourceId::new("s3.Bucket", "my-bucket"),
            ExplicitFields::Struct {
                children: HashMap::from([
                    ("authored".to_string(), ExplicitFields::Leaf),
                    ("trigger_diff".to_string(), ExplicitFields::Leaf),
                ]),
            },
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None, Some(&explicit_map));
        let projected_count = rows.iter().find_map(|r| match r {
            DetailRow::HiddenUnchanged { count } => Some(*count),
            _ => None,
        });
        assert_eq!(
            projected_count,
            Some(1),
            "With prev_explicit, server_only should not contribute to unchanged count"
        );
    }

    #[test]
    fn test_delete_with_attributes() {
        let id = ResourceId::new("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: id.clone(),
            identifier: "old-bucket".to_string(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [(
                "bucket".to_string(),
                Value::Concrete(ConcreteValue::String("old-bucket".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs), None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Attribute { key, .. } if key == "bucket"));
    }

    #[test]
    fn test_update_removed_attribute() {
        let from = State::existing(
            ResourceId::new("s3.Bucket", "my-bucket"),
            [
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("test".to_string())),
                ),
                (
                    "removed_attr".to_string(),
                    Value::Concrete(ConcreteValue::String("old-val".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        let effect = Effect::Update {
            id: ResourceId::new("s3.Bucket", "my-bucket"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["removed_attr".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Removed { key, .. } if key == "removed_attr"));
    }

    #[test]
    fn test_create_map_expanded() {
        let mut tags = IndexMap::new();
        tags.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        tags.insert(
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("prod".to_string())),
        );
        let resource = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("tags", Value::Concrete(ConcreteValue::Map(tags)));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            DetailRow::MapExpanded { key, entries } => {
                assert_eq!(key, "tags");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key, "Environment");
                assert_eq!(
                    entries[0].value,
                    Value::Concrete(ConcreteValue::String("prod".to_string()))
                );
                assert!(entries[0].annotation.is_none());
                assert_eq!(entries[1].key, "Name");
                assert_eq!(
                    entries[1].value,
                    Value::Concrete(ConcreteValue::String("test".to_string()))
                );
                assert!(entries[1].annotation.is_none());
            }
            other => panic!("expected MapExpanded, got {:?}", other),
        }
    }

    /// Regression: a Map attribute whose entry value is itself a list-of-maps
    /// must carry the raw `Value` so the renderer can pretty-print it. If
    /// the build phase pre-stringifies entry values, the inline form
    /// `[{...}, {...}]` is locked in and the renderer cannot expand it
    /// onto multiple lines (issue #2409).
    #[test]
    fn test_create_map_expanded_carries_raw_value_for_nested_list_of_maps() {
        let mut statement1 = IndexMap::new();
        statement1.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("AllowRead".to_string())),
        );
        statement1.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        let mut statement2 = IndexMap::new();
        statement2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("DenyWrite".to_string())),
        );
        statement2.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".to_string())),
        );
        let mut policy = IndexMap::new();
        policy.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".to_string())),
        );
        policy.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map(statement1)),
                Value::Concrete(ConcreteValue::Map(statement2.clone())),
            ])),
        );
        let resource = Resource::new("iam.RolePolicy", "test").with_attribute(
            "policy_document",
            Value::Concrete(ConcreteValue::Map(policy)),
        );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        let entries = rows
            .iter()
            .find_map(|r| match r {
                DetailRow::MapExpanded { key, entries } if key == "policy_document" => {
                    Some(entries)
                }
                _ => None,
            })
            .expect("expected MapExpanded row for policy_document");
        let stmt_entry = entries
            .iter()
            .find(|e| e.key == "statement")
            .expect("expected `statement` entry");
        match &stmt_entry.value {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items.len(), 2, "list-of-maps preserved as raw Value");
                assert!(
                    matches!(items[0], Value::Concrete(ConcreteValue::Map(_))),
                    "inner element kept as Value::Concrete(ConcreteValue::Map), not stringified"
                );
            }
            other => panic!(
                "expected Value::Concrete(ConcreteValue::List), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_delete_map_expanded() {
        let id = ResourceId::new("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: id.clone(),
            identifier: "old-bucket".to_string(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let mut tags = IndexMap::new();
        tags.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(tags)),
            )]
            .into_iter()
            .collect(),
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs), None);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            DetailRow::MapExpanded { key, entries } => {
                assert_eq!(key, "tags");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].key, "Name");
                assert_eq!(
                    entries[0].value,
                    Value::Concrete(ConcreteValue::String("test".to_string()))
                );
            }
            other => panic!("expected MapExpanded, got {:?}", other),
        }
    }

    #[test]
    fn delete_row_list_of_maps_emits_pretty_attribute() {
        // Regression: the Delete path used to stringify list attributes onto
        // one long line (asymmetric with build_create_rows). A list-of-maps
        // like `domain_validation_options` must route through
        // PrettyAttribute so format_value_pretty's vertical layout applies.
        let id = ResourceId::new("acm.Certificate", "cert");
        let effect = Effect::Delete {
            id: id.clone(),
            identifier: "cert".to_string(),
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let mut entry = IndexMap::new();
        entry.insert(
            "domain_name".to_string(),
            Value::Concrete(ConcreteValue::String(
                "registry-dev.carina-rs.dev".to_string(),
            )),
        );
        entry.insert(
            "validation_method".to_string(),
            Value::Concrete(ConcreteValue::String("DNS".to_string())),
        );
        let dvo = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [("domain_validation_options".to_string(), dvo)]
                .into_iter()
                .collect(),
        );

        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs), None);

        let pretty_value = rows.iter().find_map(|row| match row {
            DetailRow::PrettyAttribute { key, value } if key == "domain_validation_options" => {
                Some(value)
            }
            _ => None,
        });
        assert!(
            pretty_value.is_some(),
            "expected PrettyAttribute row for domain_validation_options, got: {rows:?}"
        );
        assert!(
            matches!(
                pretty_value.unwrap(),
                Value::Concrete(ConcreteValue::List(_))
            ),
            "PrettyAttribute should carry the raw Value::Concrete(ConcreteValue::List)"
        );
    }

    #[test]
    fn test_replace_basic() {
        let from = State::existing(
            ResourceId::new("ec2.Vpc", "my-vpc"),
            [(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
        );
        let effect = Effect::Replace {
            id: ResourceId::new("ec2.Vpc", "my-vpc"),
            from: Box::new(from),
            to,
            directives: crate::resource::Directives::default(),
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
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

        let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("vpc-".to_string()),
            InterpolationPart::Expr(refs_vpc.clone()),
        ]));
        assert!(value_references_binding(&interpolation, "vpc"));

        let function_call = Value::Deferred(DeferredValue::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::Concrete(ConcreteValue::String(",".to_string())),
                refs_vpc.clone(),
            ],
        });
        assert!(value_references_binding(&function_call, "vpc"));

        let secret = Value::Deferred(DeferredValue::Secret(Box::new(refs_vpc)));
        assert!(value_references_binding(&secret, "vpc"));

        // Closure variant removed from `Value` (issue #2230): closures
        // live on `EvalValue` and never reach this code path.

        // Still false when the binding is genuinely absent.
        assert!(!value_references_binding(
            &Value::Concrete(ConcreteValue::String("plain".to_string())),
            "vpc"
        ));
    }

    #[test]
    fn create_row_list_of_maps_emits_pretty_attribute() {
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("S1".to_string())),
        );
        entry.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        let resource = Resource::new("iam.RolePolicy", "test").with_attribute(
            "statement",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(entry),
            )])),
        );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);

        let pretty_value = rows.iter().find_map(|row| match row {
            DetailRow::PrettyAttribute { key, value } if key == "statement" => Some(value),
            _ => None,
        });
        assert!(
            pretty_value.is_some(),
            "expected PrettyAttribute row for statement, got: {rows:?}"
        );
        assert!(
            matches!(
                pretty_value.unwrap(),
                Value::Concrete(ConcreteValue::List(_))
            ),
            "PrettyAttribute should carry the raw Value::Concrete(ConcreteValue::List)"
        );
    }

    #[test]
    fn create_row_scalar_attribute_unchanged() {
        let resource = Resource::new("iam.Role", "test").with_attribute(
            "role_name",
            Value::Concrete(ConcreteValue::String("foo".to_string())),
        );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
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
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:iam::aws:policy/Policy1".to_string(),
                )),
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:iam::aws:policy/Policy2".to_string(),
                )),
            ])),
        );
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);

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
            matches!(pretty.unwrap(), Value::Concrete(ConcreteValue::List(_))),
            "PrettyAttribute should carry the raw Value::Concrete(ConcreteValue::List)"
        );
    }

    #[test]
    fn create_row_empty_list_emits_pretty_attribute() {
        // Pins down `tags = []` behavior — a regression that re-introduces
        // an `!items.is_empty()` guard would silently bypass the routing
        // for empty lists, breaking the formatting-path uniformity.
        let resource = Resource::new("iam.Role", "test")
            .with_attribute("tags", Value::Concrete(ConcreteValue::List(vec![])));
        let effect = Effect::Create(resource);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::PrettyAttribute { key, .. } if key == "tags"
            )),
            "empty list should also emit PrettyAttribute, got: {rows:?}"
        );
    }

    // --- carina#3073: schema-aware Update/Replace renderer ---

    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    /// Build a registry whose `iam.Role.policy` attribute is the
    /// IAM-policy-doc shape: a Struct with a Enum `version` and a
    /// `List<Struct{ effect: Enum }>` `statement`. `dsl_aliases`
    /// map the API spelling to the DSL alias, mirroring the real
    /// provider schema (`Allow`↔`allow`, `2012-10-17`↔`2012_10_17`).
    fn iam_policy_registry() -> SchemaRegistry {
        let effect = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Effect"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );
        let version = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Version"),
            Some(vec!["2012-10-17".to_string(), "2008-10-17".to_string()]),
            vec![
                ("2012-10-17".to_string(), "2012_10_17".to_string()),
                ("2008-10-17".to_string(), "2008_10_17".to_string()),
            ],
            None,
            None,
        );
        let statement = AttributeType::unordered_list(AttributeType::struct_(
            "Statement".to_string(),
            vec![
                StructField::new("effect", effect),
                StructField::new("action", AttributeType::string()),
            ],
        ));
        let policy = AttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![
                StructField::new("version", version),
                StructField::new("statement", statement),
            ],
        );
        let schema = ResourceSchema::new("iam.Role")
            .attribute(AttributeSchema::new("policy", policy))
            .attribute(AttributeSchema::new("description", AttributeType::string()))
            .attribute(AttributeSchema::new("region", AttributeType::string()));
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema);
        registry
    }

    fn iam_policy_value(effect: ConcreteValue, version: ConcreteValue) -> Value {
        let mut stmt = indexmap::IndexMap::new();
        stmt.insert("effect".to_string(), Value::Concrete(effect));
        stmt.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".to_string())),
        );
        let mut policy = indexmap::IndexMap::new();
        policy.insert("version".to_string(), Value::Concrete(version));
        policy.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(stmt),
            )])),
        );
        Value::Concrete(ConcreteValue::Map(policy))
    }

    /// carina#3073: state holds the DSL-alias spelling
    /// (`EnumIdentifier("allow")`/`("2012_10_17")`), desired resolves to
    /// the API-canonical `String("Allow")`/`String("2012-10-17")`. The
    /// schema types both as `Enum` with `dsl_aliases`, so
    /// `type_aware_equal` considers them equal — the renderer must emit
    /// **zero** rows, not a phantom `~ policy` / `~ effect: "allow" →
    /// "Allow"`.
    #[test]
    fn update_with_only_enum_equal_leaves_emits_no_phantom_row() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [(
                "policy".to_string(),
                iam_policy_value(
                    ConcreteValue::enum_identifier("allow".to_string()),
                    ConcreteValue::enum_identifier("2012_10_17".to_string()),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r").with_attribute(
            "policy",
            iam_policy_value(
                ConcreteValue::String("Allow".to_string()),
                ConcreteValue::String("2012-10-17".to_string()),
            ),
        );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        assert!(
            rows.is_empty(),
            "enum-equal-only policy must produce no phantom row, got: {rows:?}"
        );
    }

    /// A genuinely-changed sibling (`description`) must still render,
    /// and the enum-equal `policy` must NOT add a phantom row alongside
    /// it. Guards against the fix over-suppressing.
    #[test]
    fn update_real_sibling_change_still_renders_without_enum_phantom() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [
                (
                    "policy".to_string(),
                    iam_policy_value(
                        ConcreteValue::enum_identifier("allow".to_string()),
                        ConcreteValue::enum_identifier("2012_10_17".to_string()),
                    ),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("old".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r")
            .with_attribute(
                "policy",
                iam_policy_value(
                    ConcreteValue::String("Allow".to_string()),
                    ConcreteValue::String("2012-10-17".to_string()),
                ),
            )
            .with_attribute(
                "description",
                Value::Concrete(ConcreteValue::String("new".to_string())),
            );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string(), "description".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        assert_eq!(
            rows.len(),
            1,
            "only description should render, got: {rows:?}"
        );
        assert!(
            matches!(&rows[0], DetailRow::Changed { key, .. } if key == "description"),
            "expected description Changed row, got: {rows:?}"
        );
    }

    /// Negative: with no registry the renderer must behave exactly as
    /// before (schema-blind fallback) — the enum leaves are PartialEq
    /// unequal, so the phantom row is still produced. Pins the
    /// fallback so embedded/test callers are unaffected.
    #[test]
    fn update_without_registry_keeps_schema_blind_behavior() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [(
                "policy".to_string(),
                iam_policy_value(
                    ConcreteValue::enum_identifier("allow".to_string()),
                    ConcreteValue::enum_identifier("2012_10_17".to_string()),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r").with_attribute(
            "policy",
            iam_policy_value(
                ConcreteValue::String("Allow".to_string()),
                ConcreteValue::String("2012-10-17".to_string()),
            ),
        );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert!(
            !rows.is_empty(),
            "no-registry fallback must still show the (schema-blind) diff"
        );
    }

    /// Count/row consistency (carina#3073 Risk): in Full mode the
    /// enum-equal `policy` produces no `~` row, so it must be folded
    /// into the `HiddenUnchanged` tally instead of vanishing. A
    /// schema-blind count would treat `policy` as changed (PartialEq
    /// unequal) and the attribute would be neither rendered nor
    /// counted — the displayed total would not add up.
    #[test]
    fn update_full_mode_counts_enum_equal_attr_as_unchanged() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [(
                "policy".to_string(),
                iam_policy_value(
                    ConcreteValue::enum_identifier("allow".to_string()),
                    ConcreteValue::enum_identifier("2012_10_17".to_string()),
                ),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r").with_attribute(
            "policy",
            iam_policy_value(
                ConcreteValue::String("Allow".to_string()),
                ConcreteValue::String("2012-10-17".to_string()),
            ),
        );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Full, None, None);
        // No `~ policy` row, and `policy` is counted as 1 hidden
        // unchanged attribute — the tally adds up.
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, DetailRow::Changed { .. } | DetailRow::MapDiff { .. })),
            "no phantom policy row in Full mode, got: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| matches!(
                r,
                DetailRow::HiddenUnchanged { count } if *count == 1
            )),
            "policy must be counted as 1 hidden unchanged attr, got: {rows:?}"
        );
    }

    /// MapDiff shape (carina#3073 Risk): the IAM fixture exercises the
    /// *list-of-maps* path (sites 3/4). This pins the *Map* path (site
    /// 5 / `compute_map_diff`): a `Map<String, Enum>` attribute
    /// whose only change is an enum-equal value must produce no row.
    #[test]
    fn update_map_of_enum_enum_equal_value_no_phantom() {
        let enum_t = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec!["On".to_string(), "Off".to_string()]),
            vec![
                ("On".to_string(), "on".to_string()),
                ("Off".to_string(), "off".to_string()),
            ],
            None,
            None,
        );
        let map_t = AttributeType::map_with_key(AttributeType::string(), enum_t);
        let schema = ResourceSchema::new("x.Thing").attribute(AttributeSchema::new("modes", map_t));
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema);

        let mk = |v: ConcreteValue| {
            let mut m = indexmap::IndexMap::new();
            m.insert("a".to_string(), Value::Concrete(v));
            Value::Concrete(ConcreteValue::Map(m))
        };
        let from = State::existing(
            ResourceId::new("x.Thing", "t"),
            [(
                "modes".to_string(),
                mk(ConcreteValue::enum_identifier("on".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t")
            .with_attribute("modes", mk(ConcreteValue::String("On".to_string())));
        let effect = Effect::Update {
            id: ResourceId::new("x.Thing", "t"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["modes".to_string()],
        };
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        assert!(
            rows.is_empty(),
            "enum-equal Map<String,Enum> value must produce no row, got: {rows:?}"
        );
    }

    /// Over-suppression guard (carina#3073): a *genuine* enum change
    /// (`Allow` → `Deny`) under the registry must still render. Proves
    /// the schema-aware path equalizes only enum-equal spellings, not
    /// real enum-value changes.
    #[test]
    fn update_real_enum_change_still_renders_with_registry() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [(
                "policy".to_string(),
                iam_policy_value(
                    ConcreteValue::enum_identifier("allow".to_string()),
                    ConcreteValue::enum_identifier("2012_10_17".to_string()),
                ),
            )]
            .into_iter()
            .collect(),
        );
        // Desired flips effect to the API-canonical `Deny` — a real
        // change, not a spelling alias of `allow`.
        let to = Resource::new("iam.Role", "r").with_attribute(
            "policy",
            iam_policy_value(
                ConcreteValue::String("Deny".to_string()),
                ConcreteValue::String("2012-10-17".to_string()),
            ),
        );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        assert!(
            !rows.is_empty(),
            "a real enum change (allow → Deny) must still render a row"
        );
    }

    /// Count/row consistency, multi-attribute (carina#3073 Risk,
    /// round-2 hardening). One real change (`description`), one
    /// enum-phantom (`policy`), one truly-unchanged (`region`) → in
    /// Full mode exactly 1 rendered `~` row and a `HiddenUnchanged`
    /// of 2 (policy + region), so rendered + hidden == 3 total
    /// non-internal attributes. Guards the full tally invariant, not
    /// just `count == 1` in isolation.
    #[test]
    fn update_full_mode_multi_attr_tally_balances() {
        let from = State::existing(
            ResourceId::new("iam.Role", "r"),
            [
                (
                    "policy".to_string(),
                    iam_policy_value(
                        ConcreteValue::enum_identifier("allow".to_string()),
                        ConcreteValue::enum_identifier("2012_10_17".to_string()),
                    ),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("old".to_string())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r")
            .with_attribute(
                "policy",
                iam_policy_value(
                    ConcreteValue::String("Allow".to_string()),
                    ConcreteValue::String("2012-10-17".to_string()),
                ),
            )
            .with_attribute(
                "description",
                Value::Concrete(ConcreteValue::String("new".to_string())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".to_string())),
            );
        let effect = Effect::Update {
            id: ResourceId::new("iam.Role", "r"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["policy".to_string(), "description".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Full, None, None);

        let changed_rows = rows
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    DetailRow::Changed { .. }
                        | DetailRow::MapDiff { .. }
                        | DetailRow::ListOfMapsDiff { .. }
                )
            })
            .count();
        let hidden = rows.iter().find_map(|r| match r {
            DetailRow::HiddenUnchanged { count } => Some(*count),
            _ => None,
        });
        assert_eq!(changed_rows, 1, "only description should render: {rows:?}");
        assert_eq!(
            hidden,
            Some(2),
            "policy (phantom) + region (unchanged) must be hidden: {rows:?}"
        );
        // The displayed tally adds up: 1 rendered + 2 hidden == 3
        // total non-internal attributes.
        assert_eq!(changed_rows + hidden.unwrap(), 3);
    }

    fn modes_registry() -> SchemaRegistry {
        let mode = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );
        let schema = ResourceSchema::new("x.Thing").attribute(AttributeSchema::new(
            "modes",
            AttributeType::unordered_list(mode),
        ));
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema);
        registry
    }

    /// Real apply/plan-path element shape for a `List<Enum>`:
    /// both state (lifted by `lift_saved_state_enums`) and
    /// desired (parser-emitted per carina#2986) carry
    /// `ConcreteValue::EnumIdentifier`, NOT `String`. Tests must use
    /// this shape — a `String`-element test passes while the real path
    /// is broken (unit-test-path ≠ apply-path).
    fn enum_list(items: &[&str]) -> Value {
        Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|s| Value::Concrete(ConcreteValue::enum_identifier(s.to_string())))
                .collect(),
        ))
    }

    /// carina#3075: a `List<Enum>` attribute whose state holds the
    /// DSL-alias spelling (`["allow"]`) and whose desired resolves to
    /// API-canonical (`["Allow", "Deny"]`) — a genuine add of `Deny`
    /// co-occurring with an enum-spelling-only element. The
    /// `compute_string_list_change` set-diff was schema-blind, so it
    /// reported a phantom `- allow / + Allow` alongside the real
    /// `+ Deny`. Schema-aware: only the genuine add must render.
    /// Elements are `EnumIdentifier` (the real runtime shape).
    #[test]
    fn update_string_list_of_enum_only_genuine_add_renders() {
        let registry = modes_registry();
        let from = State::existing(
            ResourceId::new("x.Thing", "t"),
            [("modes".to_string(), enum_list(&["allow"]))]
                .into_iter()
                .collect(),
        );
        let to =
            Resource::new("x.Thing", "t").with_attribute("modes", enum_list(&["Allow", "Deny"]));
        let effect = Effect::Update {
            id: ResourceId::new("x.Thing", "t"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["modes".to_string()],
        };
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        // Exactly one StringListDiff row, adding only the genuine
        // `Deny` (the `allow`↔`Allow` element is enum-equal → not a
        // phantom add/remove).
        assert_eq!(rows.len(), 1, "expected one row, got: {rows:?}");
        let DetailRow::StringListDiff { added, removed, .. } = &rows[0] else {
            panic!("expected StringListDiff, got: {rows:?}");
        };
        assert!(
            removed.is_empty(),
            "no phantom removal of `allow`, got removed: {removed:?}"
        );
        assert_eq!(
            added.len(),
            1,
            "only the genuine `Deny` add, got added: {added:?}"
        );
        assert!(
            added[0].eq_ignore_ascii_case("deny"),
            "the genuine add is Deny, got: {added:?}"
        );
    }

    /// carina#3258: when `compute_list_of_maps_diff_parts` pairs two
    /// list elements by similarity (because exact-match in Phase 1
    /// failed on ONE field that has a true shape mismatch — e.g.
    /// `String` vs `StringList` — but `format_value` renders both sides
    /// identically), the OTHER unchanged-by-display fields must NOT
    /// appear as `ListOfMapsDiffField::Changed { old, new }` rows
    /// whose `old` and `new` are byte-identical. They must fold into
    /// `hidden_unchanged_count` like genuine unchanged fields.
    ///
    /// Real-world trigger (issue #3258 reproduction): an
    /// `awscc.iam.Role.assume_role_policy_document.statement` element
    /// has a sibling-changed peer added to the list. The existing
    /// (unchanged) OIDC statement reaches the differ with at least one
    /// shape-mismatched field (e.g. `principal.federated` as
    /// `List([String])` on one side and `StringList` on the other),
    /// which makes `Value::semantically_equal` return false and breaks
    /// Phase 1. Phase 2 pairs by similarity. Every other field
    /// `format_value`-renders identically, but the schema-blind
    /// `semantically_equal` says "different" so they all emit phantom
    /// `~ key: X → X` rows.
    #[test]
    fn list_of_maps_modified_drops_display_equal_peer_fields() {
        // `List([String])` vs `StringList(_)` both render as `["x"]`
        // via `format_value` but compare unequal under
        // `semantically_equal` (different discriminants) — the
        // shape mismatch the renderer must hide.
        let mut old_oidc = indexmap::IndexMap::new();
        old_oidc.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("sts:AssumeRoleWithWebIdentity".to_string()),
            )])),
        );
        // The shape-mismatched field that breaks Phase 1 exact match.
        old_oidc.insert(
            "principal_federated".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec![
                "arn:aws:iam::123:oidc-provider/token.example.com".to_string(),
            ])),
        );

        let mut new_oidc = indexmap::IndexMap::new();
        new_oidc.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("sts:AssumeRoleWithWebIdentity".to_string()),
            )])),
        );
        new_oidc.insert(
            "principal_federated".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String(
                    "arn:aws:iam::123:oidc-provider/token.example.com".to_string(),
                ),
            )])),
        );

        // A second wholly-new statement so the desired list grows; this
        // is what makes Phase 1's exact-match step actually fail for the
        // OIDC pair (length mismatch never reached — but the per-pair
        // check still runs and fails for the shape-mismatched field).
        let mut new_admin = indexmap::IndexMap::new();
        new_admin.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("sts:AssumeRole".to_string())),
        );

        let old_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(old_oidc),
        )]));
        let new_value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(new_oidc)),
            Value::Concrete(ConcreteValue::Map(new_admin)),
        ]));

        let (_unchanged, modified, _added, _removed) = compute_list_of_maps_diff_parts(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        // The paired OIDC element must NOT report `action` as Changed:
        // both sides render to the same `["sts:AssumeRoleWithWebIdentity"]`
        // string. The renderer cannot honestly mark it as `~`.
        for m in &modified {
            for field in m.fields.as_slice() {
                if let ListOfMapsDiffField::Changed { key, old, new } = field {
                    assert_ne!(
                        old, new,
                        "field `{key}` rendered as Changed with old == new = {old:?}; \
                         a display-equal field must fold into hidden_unchanged_count \
                         instead of emitting a phantom `~ {key}: X → X` row"
                    );
                }
            }
        }
    }

    /// carina#3258 (recursion site): inside `compute_map_diff_entries`,
    /// when `compute_map_diff` flags a key as Changed via schema-blind
    /// `semantically_equal` but the two values `format_value` to the
    /// same string, the renderer must not emit a phantom
    /// `MapDiffEntryIR::Changed { old, new }` with `old == new`.
    #[test]
    fn map_diff_entries_drops_display_equal_changed_keys() {
        // Nested map (the `principal` shape in the issue). One side has
        // a `StringList` value, the other a `List([String])` — display-
        // equal but `semantically_equal` returns false.
        let mut old_principal = indexmap::IndexMap::new();
        old_principal.insert(
            "federated".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec![
                "arn:aws:iam::123:oidc-provider/x".to_string(),
            ])),
        );
        // A genuinely different sibling key forces the parent map onto
        // the Changed path so the recursion is exercised.
        old_principal.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".to_string())),
        );

        let mut new_principal = indexmap::IndexMap::new();
        new_principal.insert(
            "federated".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("arn:aws:iam::123:oidc-provider/x".to_string()),
            )])),
        );
        new_principal.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("new.example.com".to_string())),
        );

        let old_value = Value::Concrete(ConcreteValue::Map(old_principal));
        let new_value = Value::Concrete(ConcreteValue::Map(new_principal));

        let entries = compute_map_diff_entries(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        for entry in &entries {
            if let MapDiffEntryIR::Changed { key, old, new } = entry {
                assert_ne!(
                    old, new,
                    "key `{key}` rendered as Changed with old == new = {old:?}; \
                     a display-equal map entry must be suppressed instead of \
                     emitting a phantom `~ {key}: X → X` row"
                );
            }
        }
    }

    /// carina#3258 (top-level attribute site): the same display-equal
    /// guard applies to `DetailRow::Changed` rows emitted directly for
    /// an attribute (not nested inside a list-of-maps / map-diff). A
    /// schema-blind differ that flagged the attribute as changed but
    /// renders both sides identically must fold into the
    /// `HiddenUnchanged` tally rather than emit `~ key: X → X`.
    #[test]
    fn top_level_changed_drops_display_equal_attribute() {
        // Schema-blind: no registry, so `find_changed_attributes`
        // uses `Value::semantically_equal`, which compares
        // `StringList(["x"])` to `List([String("x")])` as unequal even
        // though both `format_value` to `["x"]`.
        let from = State::existing(
            ResourceId::new("x.Thing", "t"),
            [(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec!["only-one".to_string()])),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t").with_attribute(
            "tags",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only-one".to_string()),
            )])),
        );
        let effect = Effect::Update {
            id: ResourceId::new("x.Thing", "t"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["tags".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None, None);
        for row in &rows {
            if let DetailRow::Changed { key, old, new } = row {
                assert_ne!(
                    old, new,
                    "top-level attribute `{key}` rendered as Changed with old == new = {old:?}"
                );
            }
        }
    }

    /// carina#3258 (negative — secret bypass): every secret value
    /// renders as the literal `(secret)` via `format_value`, so the
    /// display-equal guard would silently hide a secret rotation
    /// (`Secret(A) → Secret(B)`). The guard must explicitly bypass
    /// when either side contains a secret — preserving the pre-fix
    /// "uninformative but visible" `~ key: (secret) → (secret)` row
    /// rather than dropping the change entirely.
    #[test]
    fn top_level_changed_keeps_secret_rotation_visible() {
        let from = State::existing(
            ResourceId::new("x.Thing", "t"),
            [(
                "password".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("old-secret".to_string()),
                )))),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t").with_attribute(
            "password",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("new-secret".to_string()),
            )))),
        );
        let effect = Effect::Update {
            id: ResourceId::new("x.Thing", "t"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["password".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert!(
            rows.iter().any(|r| matches!(
                r,
                DetailRow::Changed { key, .. } if key == "password"
            )),
            "secret rotation must still produce a Changed row, got: {rows:?}"
        );
    }

    /// carina#3258 (regression caught from real-infra repro): when a
    /// paired list-of-maps element has nested-map peer fields
    /// (`condition`, `principal`) whose every child entry is itself a
    /// display-equal phantom suppressed by the inner
    /// `compute_map_diff_entries`, the *parent* must NOT push a
    /// `NestedMapChanged { entries: [] }` row whose only visible
    /// output is `~ key:` with an empty body. Fold such empty nested
    /// fields into the per-element hidden-unchanged count.
    #[test]
    fn list_of_maps_modified_drops_empty_nested_map_field() {
        // Inner map whose only "change" is a shape mismatch that
        // collapses to display-equal under the phantom guard.
        let mut old_condition = indexmap::IndexMap::new();
        old_condition.insert(
            "string_equals".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()])),
        );
        let mut new_condition = indexmap::IndexMap::new();
        new_condition.insert(
            "string_equals".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("x".to_string()),
            )])),
        );

        // The whole statement mirrors the real OIDC repro: many
        // shared fields (`sid`, `effect`, `action`) drive Phase 2
        // similarity high enough to pair; the `condition` nested map
        // is the all-phantom field that triggers the bug.
        let mut old_stmt = indexmap::IndexMap::new();
        old_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".to_string())),
        );
        old_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        old_stmt.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(old_condition)),
        );

        let mut new_stmt = indexmap::IndexMap::new();
        new_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".to_string())),
        );
        new_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        new_stmt.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(new_condition)),
        );

        let old_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(old_stmt),
        )]));
        let new_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(new_stmt),
        )]));

        let (_unchanged, modified, _added, _removed) = compute_list_of_maps_diff_parts(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        for m in &modified {
            for field in m.fields.as_slice() {
                if let ListOfMapsDiffField::NestedMapChanged { key, entries } = field {
                    assert!(
                        !entries.is_empty(),
                        "nested-map field `{key}` rendered with empty entries; \
                         an all-phantom nested map must fold into hidden_unchanged_count, \
                         not push a `~ {key}:` header with no children"
                    );
                }
            }
        }
    }

    /// carina#3258 (negative — secret bypass at map-diff site):
    /// mirrors `top_level_changed_keeps_secret_rotation_visible` at
    /// `compute_map_diff_entries`. A nested `Secret(A) → Secret(B)`
    /// inside a `MapDiff` must not be silently dropped.
    #[test]
    fn map_diff_entries_keeps_secret_rotation_visible() {
        let mut old_map = indexmap::IndexMap::new();
        old_map.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("old".to_string()),
            )))),
        );
        let mut new_map = indexmap::IndexMap::new();
        new_map.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("new".to_string()),
            )))),
        );
        let old_value = Value::Concrete(ConcreteValue::Map(old_map));
        let new_value = Value::Concrete(ConcreteValue::Map(new_map));
        let entries = compute_map_diff_entries(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );
        assert!(
            entries.iter().any(|e| matches!(
                e,
                MapDiffEntryIR::Changed { key, .. } if key == "password"
            )),
            "secret rotation under a map-diff must still produce a Changed entry, got: {entries:?}"
        );
    }

    /// carina#3258 (negative — secret bypass at list-of-maps site):
    /// mirrors the above for `compute_list_of_maps_diff_parts`. Two
    /// statements paired by similarity where the unchanged-by-display
    /// peer holds a rotated secret must keep that peer as a Changed
    /// field, not fold into the hidden-count.
    #[test]
    fn list_of_maps_modified_keeps_secret_rotation_visible() {
        let secret_old = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("old".to_string()),
        ))));
        let secret_new = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("new".to_string()),
        ))));

        // Same `sid` on both sides so `map_similarity > 0` → Phase 2
        // pairs them. A different `effect` key forces Phase 1
        // exact-match to fail so the pair is *modified*, not unchanged.
        let mut old_stmt = indexmap::IndexMap::new();
        old_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".to_string())),
        );
        old_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        old_stmt.insert("password".to_string(), secret_old);

        let mut new_stmt = indexmap::IndexMap::new();
        new_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".to_string())),
        );
        new_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".to_string())),
        );
        new_stmt.insert("password".to_string(), secret_new);

        let old_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(old_stmt),
        )]));
        let new_value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(new_stmt),
        )]));

        let (_unchanged, modified, _added, _removed) = compute_list_of_maps_diff_parts(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        let password_changed = modified.iter().any(|m| {
            m.fields
                .as_slice()
                .iter()
                .any(|f| matches!(f, ListOfMapsDiffField::Changed { key, .. } if key == "password"))
        });
        assert!(
            password_changed,
            "secret rotation as a paired-element peer must keep its Changed field, got: {modified:?}"
        );
    }

    /// Negative (carina#3075, mirrors the carina#3073 fallback
    /// convention): with no registry, `compute_string_list_change`
    /// stays schema-blind — the raw-spelling set-diff still reports the
    /// phantom `- allow` + `+ Allow` alongside the genuine `+ Deny`.
    /// Pins that the canonicalization only engages when a schema is
    /// available. Same real `EnumIdentifier` element shape.
    #[test]
    fn update_string_list_of_enum_no_registry_keeps_schema_blind() {
        let from = State::existing(
            ResourceId::new("x.Thing", "t"),
            [("modes".to_string(), enum_list(&["allow"]))]
                .into_iter()
                .collect(),
        );
        let to =
            Resource::new("x.Thing", "t").with_attribute("modes", enum_list(&["Allow", "Deny"]));
        let effect = Effect::Update {
            id: ResourceId::new("x.Thing", "t"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["modes".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1, "expected one row, got: {rows:?}");
        let DetailRow::StringListDiff { added, removed, .. } = &rows[0] else {
            panic!("expected StringListDiff, got: {rows:?}");
        };
        // Schema-blind: `allow` (state) is not in the new raw set
        // {`Allow`,`Deny`} → removed; both `Allow` and `Deny` are not
        // in the old raw set {`allow`} → added.
        assert_eq!(removed, &vec!["allow".to_string()], "schema-blind removal");
        assert_eq!(
            added,
            &vec!["Allow".to_string(), "Deny".to_string()],
            "schema-blind additions"
        );
    }
}
