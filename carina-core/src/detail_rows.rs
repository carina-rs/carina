//! Output-neutral intermediate representation for plan detail rows.
//!
//! Both CLI and TUI consume `DetailRow` values to render effect details.
//! The shared row builders encapsulate all logic for deciding
//! which rows to show for a given effect, keeping rendering frontends thin.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::diff_helpers::{
    compute_map_diff, compute_unchanged_count, list_element_type, map_entry_subtype,
    schema_aware_equal,
};
use crate::differ::{
    AttrComparison, ComparisonContext, LeafProvenance, TypedAttr, key_should_enter_patch,
};
use crate::effect::{DeletedInstanceKey, Effect, EffectGeneration};
use crate::explicit::{ExplicitProjection, ProjectedAuthoring};
use crate::non_empty::NonEmptyVec;
use crate::plan::{Plan, ReplaceDisplayInfo};
use crate::provider::{ComparisonResolutionError, ProviderResult, ProviderRouter};
use crate::provider::{NestedWriteOnlyRecord, PersistedAttributes};
use crate::resource::{
    CanonicalComparisonAttributes, ConcreteValue, DefaultTagKeys, DeferredValue, ResourceId, Value,
    pair_list_elements,
};
use crate::schema::{
    CompiledAttributeType as AttributeType, CompiledResourceSchema as ResourceSchema,
    SchemaRegistry, empty_defs_for_schema_walks,
};
use crate::value::{
    PrettyValue, RenderedKey, RenderedValue, is_list_of_maps, render_enum_text, render_other,
    render_string_text, render_value,
};

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

/// Historical attribute values prepared exclusively for display.
///
/// The maps are private and the only constructors mask provider plaintext
/// using either the desired secret shape or the persisted state shape. This
/// keeps update/replacement/delete renderers from ever accepting raw provider
/// state directly, and keeps the update view out of the deleted-instance map.
#[derive(Debug, Clone)]
pub struct HistoricalDisplayAttributes {
    updates: HashMap<ResourceId, HistoricalUpdateAttributes>,
    deletes: HashMap<DeletedInstanceKey, HistoricalDeleteAttributes>,
    releases: HashMap<ResourceId, Vec<HistoricalListKeyRelease>>,
}

#[derive(Debug, Clone)]
struct HistoricalListKeyRelease {
    attribute: String,
    key_field: String,
    keys: HashSet<String>,
}

#[derive(Debug, Clone)]
struct HistoricalUpdateAttributes {
    attributes: HashMap<String, Value>,
    stored_attributes: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone)]
struct HistoricalDeleteAttributes {
    attributes: HashMap<String, Value>,
    stored_attributes: Option<HashMap<String, Value>>,
    nested_write_only: NestedWriteOnlyRecord,
}

impl HistoricalDisplayAttributes {
    /// Build the fail-safe display view available from a plan alone. Desired
    /// secret shapes mask their historical counterparts; callers with state
    /// provenance should then refine it through the persisted-shape methods.
    pub fn for_plan<S>(plan: &Plan<S>) -> Self {
        let mut display = Self {
            updates: HashMap::new(),
            deletes: HashMap::new(),
            releases: HashMap::new(),
        };

        for effect in plan.effects() {
            match effect {
                Effect::ReleaseListKeys {
                    id,
                    attribute,
                    key_field,
                    keys,
                } => display
                    .releases
                    .entry(id.as_inner().clone())
                    .or_default()
                    .push(HistoricalListKeyRelease {
                        attribute: attribute.clone(),
                        key_field: key_field.as_str().to_string(),
                        keys: keys.iter().map(|key| key.key().to_string()).collect(),
                    }),
                Effect::Read { .. }
                | Effect::Create(_)
                | Effect::ResumeCreate { .. }
                | Effect::Update { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::Wait { .. }
                | Effect::DeferredCreate(_)
                | Effect::DeferredReplace(_) => {}
            }
        }

        for effect in plan.effects() {
            match effect {
                Effect::Update { from, to, .. } => {
                    let attributes = display.filter_released(&from.id, from.attributes.clone());
                    display.updates.insert(
                        from.id.clone(),
                        HistoricalUpdateAttributes {
                            attributes: mask_attributes_using_secret_shape(
                                &attributes,
                                &to.attributes,
                            ),
                            stored_attributes: None,
                        },
                    );
                }
                Effect::Delete {
                    id,
                    identifier,
                    generation,
                    ..
                } => {
                    display.deletes.insert(
                        DeletedInstanceKey::new(
                            id.as_inner().clone(),
                            generation.clone(),
                            identifier.to_string(),
                        ),
                        HistoricalDeleteAttributes {
                            attributes: HashMap::new(),
                            stored_attributes: None,
                            nested_write_only: NestedWriteOnlyRecord::new(),
                        },
                    );
                }
                Effect::Read { .. }
                | Effect::Create(_)
                | Effect::ResumeCreate { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. }
                | Effect::DeferredCreate(_)
                | Effect::DeferredReplace(_) => {}
            }
        }

        for replacement in plan.replace_display_info() {
            let Some(Effect::Create(to)) = plan.effects().get(replacement.create_idx) else {
                continue;
            };
            let Some(Effect::Delete {
                id,
                identifier,
                generation,
                ..
            }) = plan.effects().get(replacement.delete_idx)
            else {
                continue;
            };
            display.deletes.insert(
                DeletedInstanceKey::new(
                    id.as_inner().clone(),
                    generation.clone(),
                    identifier.to_string(),
                ),
                HistoricalDeleteAttributes {
                    attributes: mask_attributes_using_secret_shape(
                        replacement.previous_attributes,
                        &to.attributes,
                    ),
                    stored_attributes: None,
                    nested_write_only: NestedWriteOnlyRecord::new(),
                },
            );
        }

        display
    }

    fn filter_released(
        &self,
        id: &ResourceId,
        mut attributes: HashMap<String, Value>,
    ) -> HashMap<String, Value> {
        let Some(releases) = self.releases.get(id) else {
            return attributes;
        };
        for release in releases {
            let Some(Value::Concrete(ConcreteValue::List(elements))) =
                attributes.get_mut(&release.attribute)
            else {
                continue;
            };
            elements.retain(|element| {
                let Value::Concrete(ConcreteValue::Map(fields)) = element else {
                    return true;
                };
                let Some(Value::Concrete(ConcreteValue::String(key))) =
                    fields.get(&release.key_field)
                else {
                    return true;
                };
                !release.keys.contains(key.expose_plaintext())
            });
        }
        attributes
    }

    /// Refine an Update's old-side view using persisted secret shape without
    /// discarding the desired-secret mask established by [`Self::for_plan`].
    pub fn set_update_from_provider_and_persisted(
        &mut self,
        id: ResourceId,
        provider_attributes: &HashMap<String, Value>,
        persisted_attributes: Option<&PersistedAttributes>,
    ) {
        let filtered_provider = self.filter_released(&id, provider_attributes.clone());
        let persisted_masked =
            mask_attributes_using_persisted_shape(&filtered_provider, persisted_attributes);
        let desired_shape = self.updates.get(&id).map(|entry| &entry.attributes);
        let masked = combine_with_desired_secret_shape(persisted_masked, desired_shape);
        // Keep the stored anchor unfiltered: its list indices must stay
        // aligned with `prev_explicit::ListElements` so projection can still
        // hide server-added defaults. Only the provider/current comparison
        // view drops released keys.
        let stored_attributes = persisted_attributes.map(PersistedAttributes::to_values);
        self.updates.insert(
            id,
            HistoricalUpdateAttributes {
                attributes: masked,
                stored_attributes,
            },
        );
    }

    /// Replace a Delete generation's view using provider values and the
    /// persisted JSON secret shape as masking authority.
    pub fn set_delete_from_provider_and_persisted(
        &mut self,
        key: DeletedInstanceKey,
        provider_attributes: &HashMap<String, Value>,
        persisted_attributes: Option<&PersistedAttributes>,
        nested_write_only: &NestedWriteOnlyRecord,
    ) {
        let persisted_masked =
            mask_attributes_using_persisted_shape(provider_attributes, persisted_attributes);
        let desired_shape = self.deletes.get(&key).map(|entry| &entry.attributes);
        let masked = combine_with_desired_secret_shape(persisted_masked, desired_shape);
        self.deletes.insert(
            key,
            HistoricalDeleteAttributes {
                attributes: masked,
                stored_attributes: persisted_attributes.map(PersistedAttributes::to_values),
                nested_write_only: nested_write_only.clone(),
            },
        );
    }

    /// Decode a deposed generation directly from persisted JSON. Valid
    /// `$secret` tags become typed redacted values before reaching a renderer.
    pub fn set_delete_from_persisted(
        &mut self,
        key: DeletedInstanceKey,
        persisted_attributes: &PersistedAttributes,
    ) {
        let persisted = persisted_attributes.to_values();
        let desired_shape = self.deletes.get(&key).map(|entry| &entry.attributes);
        let masked = combine_with_desired_secret_shape(persisted, desired_shape);
        self.deletes.insert(
            key,
            HistoricalDeleteAttributes {
                attributes: masked,
                stored_attributes: Some(persisted_attributes.to_values()),
                // State v18 introduced provenance only for the current
                // instance; historical generations have no leaf record.
                nested_write_only: NestedWriteOnlyRecord::new(),
            },
        );
    }

    fn update(&self, id: &ResourceId) -> Option<&HistoricalUpdateAttributes> {
        self.updates.get(id)
    }

    fn delete(&self, key: &DeletedInstanceKey) -> Option<&HistoricalDeleteAttributes> {
        self.deletes.get(key)
    }
}

fn mask_attributes_using_secret_shape(
    plaintext: &HashMap<String, Value>,
    shape: &IndexMap<String, Value>,
) -> HashMap<String, Value> {
    plaintext
        .iter()
        .map(|(key, value)| {
            let value = shape.get(key).map_or_else(
                || value.clone(),
                |shape| crate::value::mask_plaintext_using_secret_shape(value, shape),
            );
            (key.clone(), value)
        })
        .collect()
}

fn mask_attributes_using_persisted_shape(
    plaintext: &HashMap<String, Value>,
    shape: Option<&PersistedAttributes>,
) -> HashMap<String, Value> {
    plaintext
        .iter()
        .map(|(key, value)| {
            let value = shape.map_or_else(
                || value.clone(),
                |shape| crate::value::mask_plaintext_using_persisted_attribute(value, shape, key),
            );
            (key.clone(), value)
        })
        .collect()
}

fn mask_attributes_using_value_shape(
    plaintext: &HashMap<String, Value>,
    shape: &HashMap<String, Value>,
) -> HashMap<String, Value> {
    plaintext
        .iter()
        .map(|(key, value)| {
            let value = shape.get(key).map_or_else(
                || value.clone(),
                |shape| crate::value::mask_plaintext_using_secret_shape(value, shape),
            );
            (key.clone(), value)
        })
        .collect()
}

fn combine_with_desired_secret_shape(
    values: HashMap<String, Value>,
    desired_shape: Option<&HashMap<String, Value>>,
) -> HashMap<String, Value> {
    desired_shape.map_or_else(
        || values.clone(),
        |shape| mask_attributes_using_value_shape(&values, shape),
    )
}

/// Output-neutral representation of a detail row for plan display.
/// Both CLI and TUI consume these to render effect details.
#[derive(Debug, Clone, PartialEq)]
pub enum DetailRow {
    /// A preformatted informational line.
    Text { text: String },
    /// A provider-retained keyed list entry released from authored state.
    ReleasedListKey {
        attribute: String,
        key: String,
        retained_value: RenderedValue,
    },
    /// A normal attribute value (for Create/Delete effects)
    Attribute {
        key: String,
        value: RenderedValue,
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
    /// An attribute whose value is rendered with indentation supplied by the
    /// frontend while the formatting itself remains owned by core.
    PrettyAttribute { key: String, value: PrettyValue },
    /// An attribute that changed (for Update effects)
    Changed {
        key: String,
        old: RenderedValue,
        new: RenderedValue,
    },
    /// An attribute that changed and forces replacement.
    ChangedForcesReplacement {
        key: String,
        old: RenderedValue,
        new: RenderedValue,
    },
    /// A map attribute with key-level diffs.
    MapDiff {
        key: String,
        /// `entries` is `NonEmptyVec`: a Map with no displayable entries
        /// is dropped at the IR builder (#2910).
        entries: NonEmptyVec<MapDiffEntryIR>,
    },
    /// A map attribute with key-level diffs that force replacement.
    MapDiffForcesReplacement {
        key: String,
        /// `entries` is `NonEmptyVec`: a Map with no displayable entries
        /// is dropped at the IR builder (#2910).
        entries: NonEmptyVec<MapDiffEntryIR>,
    },
    /// A list-of-maps diff.
    ListOfMapsDiff {
        key: String,
        /// `block` enforces at least one displayable section (#2910).
        block: NonEmptyListOfMapsBlock,
    },
    /// A list-of-maps diff that forces replacement.
    ListOfMapsDiffForcesReplacement {
        key: String,
        /// `block` enforces at least one displayable section (#2910).
        block: NonEmptyListOfMapsBlock,
    },
    /// A replacement-forcing map whose nested diff has no displayable entries.
    ForceReplaceMapHeader { key: String },
    /// A replacement-forcing list-of-maps whose nested diff has no displayable entries.
    ForceReplaceListOfMapsHeader { key: String },
    /// Per-element `List<String>` diff (#2943).
    StringListDiff {
        key: String,
        unchanged: Vec<RenderedValue>,
        added: Vec<RenderedValue>,
        removed: Vec<RenderedValue>,
    },
    /// Per-element `List<String>` diff that forces replacement.
    StringListDiffForcesReplacement {
        key: String,
        unchanged: Vec<RenderedValue>,
        added: Vec<RenderedValue>,
        removed: Vec<RenderedValue>,
    },
    /// An attribute that was removed (for Update effects)
    Removed { key: String, old: RenderedValue },
    /// An attribute with a schema default value (for Create effects in Full mode)
    Default { key: String, value: RenderedValue },
    /// A read-only attribute (known after apply, for Create effects in Full mode)
    ReadOnly { key: String },
    /// Summary of hidden unchanged attributes (for Update/Replace effects in Full mode)
    HiddenUnchanged { count: usize },
    /// A removed create-only attribute that forces replacement.
    ReplaceRemoved { key: String, old: RenderedValue },
    /// A cascade-triggered create-only attribute (value not yet known)
    ReplaceCascade {
        key: String,
        old: RenderedValue,
        new: RenderedValue,
    },
    /// Temporary name note for create-before-destroy replacement.
    TemporaryNameNote {
        can_rename: bool,
        temporary_value: RenderedValue,
        original_value: RenderedValue,
        attribute: String,
    },
}

/// A single entry in an expanded map (e.g., tags with default_tags annotation).
///
/// The value retains its structure behind [`PrettyValue`] so frontends can
/// supply their actual indentation without gaining access to the raw value.
#[derive(Debug, Clone, PartialEq)]
pub struct MapExpandedEntry {
    pub key: RenderedKey,
    pub value: PrettyValue,
    /// Optional annotation (e.g., "# default_tags") shown after the value
    pub annotation: Option<String>,
}

/// A map diff entry (key-level diff)
#[derive(Debug, Clone, PartialEq)]
pub enum MapDiffEntryIR {
    Added {
        key: RenderedKey,
        value: RenderedValue,
        /// Optional annotation (e.g., "# default_tags") shown after the value.
        annotation: Option<String>,
    },
    Removed {
        key: RenderedKey,
        value: RenderedValue,
    },
    Changed {
        key: RenderedKey,
        old: RenderedValue,
        new: RenderedValue,
        /// Optional annotation (e.g., "# default_tags") shown after the new value.
        annotation: Option<String>,
    },
    /// Nested map diff: when both old and new values are maps,
    /// recursively compute key-level diffs instead of showing as one-liner.
    /// `entries` is `NonEmptyVec` so renderers do not need to defend
    /// against an empty container header (#2910).
    NestedMapDiff {
        key: RenderedKey,
        entries: NonEmptyVec<MapDiffEntryIR>,
    },
    /// Nested list-of-maps diff: when both old and new values are lists of maps,
    /// show per-item field-level diffs instead of one-liner. `block`'s
    /// constructor enforces "at least one of unchanged/modified/added/removed
    /// is non-empty" (#2910).
    NestedListOfMapsDiff {
        key: RenderedKey,
        block: NonEmptyListOfMapsBlock,
    },
    /// Per-element `List<String>` field diff inside a nested map (#3234).
    /// Mirrors `ListOfMapsDiffField::StringListChanged` but lives at the
    /// map-entry layer so a list-valued struct field (e.g.
    /// `principal.aws`) renders as multi-line `+` / `-` markers instead
    /// of the inline `[A] → [A, B]` form that overflows the terminal.
    StringListChanged {
        key: RenderedKey,
        unchanged: Vec<RenderedValue>,
        added: Vec<RenderedValue>,
        removed: Vec<RenderedValue>,
    },
}

/// A list-of-maps diff bundle whose constructor refuses the
/// "everything empty" shape (#2910). The IR builder converts via
/// [`NonEmptyListOfMapsBlock::from_parts`] and only pushes the parent
/// row when conversion succeeds, so renderers never see a section
/// header with nothing under it.
#[derive(Debug, Clone, PartialEq)]
pub struct NonEmptyListOfMapsBlock {
    unchanged: Vec<RenderedValue>,
    modified: Vec<ListOfMapsDiffModified>,
    added: Vec<ListOfMapsDiffItem>,
    removed: Vec<ListOfMapsDiffItem>,
}

impl NonEmptyListOfMapsBlock {
    pub fn from_parts(
        unchanged: Vec<RenderedValue>,
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

    pub fn unchanged(&self) -> &[RenderedValue] {
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
/// Fields retain their structure behind [`PrettyValue`] so renderers can
/// supply their actual indent column without gaining access to raw values.
#[derive(Debug, Clone, PartialEq)]
pub struct ListOfMapsDiffItem {
    /// Map fields in the order they should be rendered (alphabetical).
    pub fields: Vec<(RenderedKey, PrettyValue)>,
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
        key: RenderedKey,
        old: RenderedValue,
        new: RenderedValue,
    },
    /// Field value is a nested map that changed — show recursive key-level diffs
    NestedMapChanged {
        key: RenderedKey,
        entries: Vec<MapDiffEntryIR>,
    },
    /// Per-element `List<String>` field diff inside a modified
    /// list-of-maps element (#2943).
    StringListChanged {
        key: RenderedKey,
        unchanged: Vec<RenderedValue>,
        added: Vec<RenderedValue>,
        removed: Vec<RenderedValue>,
    },
}

/// Build detail rows for an effect using delete attributes keyed by resource
/// id plus generation.
pub fn build_detail_rows_by_instance<S>(
    effect: &Effect<S>,
    registry: Option<&SchemaRegistry>,
    detail: DetailLevel,
    historical_attributes: &HistoricalDisplayAttributes,
    prev_explicit: Option<&HashMap<ResourceId, crate::explicit::ExplicitFields>>,
    provider: &ProviderRouter,
) -> ProviderResult<Vec<DetailRow>> {
    if detail == DetailLevel::NamesOnly {
        return Ok(Vec::new());
    }

    // carina#3181: `Effect` payloads are typestate structs — the
    // managed variants carry `Resource`, `Read` carries a
    // `DataSource`. Schema lookup routes through the matching
    // `get_for` / `get_for_data_source` registry method.
    Ok(match effect {
        Effect::Create(r) | Effect::ResumeCreate { resource: r, .. } => {
            let schema = registry
                .and_then(|registry| registry.get_for(r))
                .map(|schema| schema.schema().as_schema());
            build_create_rows(&r.attributes, &r.default_tag_keys, schema, detail)
        }
        Effect::Update {
            from,
            to,
            changed_attributes,
            ..
        } => {
            let explicit = prev_explicit.and_then(|map| map.get(&to.id));
            let Some(historical) = historical_attributes.update(&from.id) else {
                return Ok(Vec::new());
            };
            let registry = registry.ok_or_else(|| {
                ComparisonResolutionError::MissingResourceSchema { id: to.id.clone() }
            })?;
            let resolved = provider.comparison_for(registry, &to.id)?;
            build_update_rows(
                historical,
                to,
                changed_attributes,
                Some(resolved.owner_schema().schema().as_schema()),
                detail,
                explicit,
                *resolved.context(),
            )
        }
        Effect::Delete {
            id,
            identifier,
            generation,
            ..
        } => build_delete_rows(id, identifier.as_str(), generation, historical_attributes),
        Effect::Read { resource } => {
            let schema = registry
                .and_then(|registry| registry.get_for_data_source(resource))
                .map(|schema| schema.schema().as_schema());
            build_create_rows(
                &resource.attributes,
                &DefaultTagKeys::default(),
                schema,
                detail,
            )
        }
        Effect::Import { identifier, .. } => {
            // carina#3329: the identifier is carried as a `Value` so a
            // deferred upstream-state reference inside a `"${X.attr}|…"`
            // interpolation renders as `(known after upstream apply: …)`
            // instead of being silently substituted to empty.
            // `format_import_identifier` prints concrete identifiers bare
            // and falls back to the structured canonical value formatter
            // shape only for the deferred / interpolation case.
            vec![DetailRow::Attribute {
                key: "id".to_string(),
                value: render_other(crate::effect::format_import_identifier(identifier)),
                ref_binding: None,
                annotation: None,
            }]
        }
        Effect::ReleaseListKeys {
            attribute, keys, ..
        } => keys
            .iter()
            .map(|key| DetailRow::ReleasedListKey {
                attribute: attribute.clone(),
                key: key.key().to_string(),
                retained_value: render_value(key.retained_value()),
            })
            .collect(),
        Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::Wait { .. }
        | Effect::DeferredCreate(_)
        | Effect::DeferredReplace(_) => Vec::new(),
    })
}

/// Build the TUI-only identifier fallback for a delete whose normal detail
/// rows are empty.
///
/// Taking the typed effect keeps arbitrary text from being promoted into a
/// [`RenderedValue`] by downstream renderers. Callers remain responsible for
/// deciding whether their display surface should show this fallback.
pub fn delete_identifier_fallback_row(effect: &Effect) -> Option<DetailRow> {
    let Effect::Delete { identifier, .. } = effect else {
        return None;
    };
    Some(DetailRow::Attribute {
        key: "identifier".to_string(),
        value: render_other(identifier.to_string()),
        ref_binding: None,
        annotation: None,
    })
}

fn build_create_rows(
    attributes: &IndexMap<String, Value>,
    default_tag_keys: &DefaultTagKeys,
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    let mut keys: Vec<_> = attributes.keys().collect();
    keys.sort();

    for key in &keys {
        let value = &attributes[*key];
        // Expand tags map into individual rows with default_tags annotation
        if key.as_str() == "tags"
            && !default_tag_keys.is_empty()
            && let Value::Concrete(ConcreteValue::Map(map)) = value
        {
            rows.push(build_expanded_tags_row(map, default_tag_keys));
            continue;
        }
        // `Value::Concrete(ConcreteValue::List)` (any element type) goes through PrettyAttribute so
        // `PrettyValue::render` can apply its 80-col threshold and YAML-style
        // vertical layout. `Value::Concrete(ConcreteValue::Map)` keeps the existing MapExpanded path
        // because that variant carries per-entry annotation slots that
        // PrettyAttribute does not represent (used by tags/default_tags).
        if let Value::Concrete(ConcreteValue::List(_)) = value {
            rows.push(DetailRow::PrettyAttribute {
                key: key.to_string(),
                value: PrettyValue::new(value.clone()),
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
                value: render_value(value),
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
            key: RenderedKey::from_raw(k),
            value: PrettyValue::new(map[k].clone()),
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
    default_tag_keys: &DefaultTagKeys,
) -> DetailRow {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let entries = keys
        .into_iter()
        .map(|key| {
            let value = &map[key];
            let annotation = default_tag_annotation(default_tag_keys, key);
            MapExpandedEntry {
                key: RenderedKey::from_raw(key),
                value: PrettyValue::new(value.clone()),
                annotation,
            }
        })
        .collect();
    DetailRow::MapExpanded {
        key: "tags".to_string(),
        entries,
    }
}

fn default_tag_annotation(default_tag_keys: &DefaultTagKeys, key: &str) -> Option<String> {
    default_tag_keys
        .contains(key)
        .then(|| "# default_tags".to_string())
}

fn build_update_rows<S>(
    historical: &HistoricalUpdateAttributes,
    to: &crate::resource::Resource<S>,
    changed_attributes: &[String],
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
    explicit: Option<&crate::explicit::ExplicitFields>,
    comparison: ComparisonContext<'_>,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    let from_attributes = match schema {
        Some(schema) => {
            CanonicalComparisonAttributes::canonicalize(historical.attributes.clone(), schema)
        }
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(historical.attributes.clone()),
        ),
    };
    let stored_attributes = historical
        .stored_attributes
        .clone()
        .map(|attributes| match schema {
            Some(schema) => CanonicalComparisonAttributes::canonicalize(attributes, schema),
            None => CanonicalComparisonAttributes::from_unschematized(
                crate::resource::UnschematizedComparisonAttributes::new(attributes),
            ),
        });
    let comparison_attributes = match schema {
        Some(schema) => {
            CanonicalComparisonAttributes::canonicalize(to.resolved_attributes(), schema)
        }
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(to.resolved_attributes()),
        ),
    };
    let comparison_attributes = to.resolved_ignore_changes.mask_comparison_readable_leaves(
        &comparison_attributes,
        &from_attributes,
        schema,
    );
    let defs = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());
    let projected = project_from_attrs(
        &from_attributes,
        stored_attributes.as_ref(),
        &comparison_attributes,
        explicit,
        schema,
        comparison,
    );
    let from_attrs_projected = projected.attributes();

    let attr_rows = build_attribute_diff_rows(
        to,
        &comparison_attributes,
        AttributeDiffContext::for_update(
            &from_attributes,
            from_attrs_projected,
            schema,
            defs,
            detail,
            changed_attributes,
            comparison,
        )
        .with_explicit(Some(&projected.authoring)),
    );
    rows.extend(attr_rows.rows);

    // Show removed attributes (in changed_attributes but not in to).
    // Use the projected map so we don't surface unauthored leaves as
    // removals (refs awscc#206).
    let mut removed_keys: Vec<_> = changed_attributes
        .iter()
        .filter(|key| !to.attributes.contains_key(key.as_str()))
        .collect();
    removed_keys.sort();
    for key in removed_keys {
        if let Some(old_value) = from_attrs_projected.get(key.as_str()) {
            let attr_type = schema
                .and_then(|s| s.attributes.get(key.as_str()))
                .map(|a| &a.attr_type);
            rows.push(build_removed_attr_row(
                key, old_value, attr_type, defs, detail, comparison,
            ));
        }
    }

    // In Full mode, show count of unchanged attributes hidden
    if detail == DetailLevel::Full {
        let unchanged_count = compute_unchanged_count(
            from_attrs_projected,
            &comparison_attributes,
            None,
            schema,
            &comparison,
        ) + attr_rows.effectively_unchanged;
        if unchanged_count > 0 {
            rows.push(DetailRow::HiddenUnchanged {
                count: unchanged_count,
            });
        }
    }

    rows
}

struct AttributeDiffRows {
    rows: Vec<DetailRow>,
    effectively_unchanged: usize,
}

fn changed_row(
    key: String,
    old: RenderedValue,
    new: RenderedValue,
    forces_replacement: bool,
) -> DetailRow {
    if forces_replacement {
        DetailRow::ChangedForcesReplacement { key, old, new }
    } else {
        DetailRow::Changed { key, old, new }
    }
}

fn map_diff_row(
    key: String,
    entries: NonEmptyVec<MapDiffEntryIR>,
    forces_replacement: bool,
) -> DetailRow {
    if forces_replacement {
        DetailRow::MapDiffForcesReplacement { key, entries }
    } else {
        DetailRow::MapDiff { key, entries }
    }
}

fn list_of_maps_diff_row(
    key: String,
    block: NonEmptyListOfMapsBlock,
    forces_replacement: bool,
) -> DetailRow {
    if forces_replacement {
        DetailRow::ListOfMapsDiffForcesReplacement { key, block }
    } else {
        DetailRow::ListOfMapsDiff { key, block }
    }
}

fn string_list_diff_row(
    key: String,
    unchanged: Vec<RenderedValue>,
    added: Vec<RenderedValue>,
    removed: Vec<RenderedValue>,
    forces_replacement: bool,
) -> DetailRow {
    if forces_replacement {
        DetailRow::StringListDiffForcesReplacement {
            key,
            unchanged,
            added,
            removed,
        }
    } else {
        DetailRow::StringListDiff {
            key,
            unchanged,
            added,
            removed,
        }
    }
}

struct AttributeDiffContext<'a> {
    from_attrs_raw: &'a CanonicalComparisonAttributes,
    from_attrs_projected: &'a CanonicalComparisonAttributes,
    schema: Option<&'a ResourceSchema>,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    leaf_provenance: DetailLeafProvenance<'a>,
    forces_replacement_keys: Option<&'a HashSet<&'a str>>,
    cascade_ref_hints: &'a [(String, String)],
    explicit: Option<&'a ProjectedAuthoring>,
    comparison: ComparisonContext<'a>,
}

#[derive(Clone, Copy)]
enum DetailLeafProvenance<'a> {
    DecidedByPlan(&'a [String]),
    Recorded(&'a NestedWriteOnlyRecord),
}

struct ReplacementDiffContext<'a> {
    nested_write_only: &'a NestedWriteOnlyRecord,
    forces_replacement_keys: &'a HashSet<&'a str>,
    cascade_ref_hints: &'a [(String, String)],
}

impl<'a> AttributeDiffContext<'a> {
    fn for_update(
        from_attrs_raw: &'a CanonicalComparisonAttributes,
        from_attrs_projected: &'a CanonicalComparisonAttributes,
        schema: Option<&'a ResourceSchema>,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        detail: DetailLevel,
        changed_attribute_keys: &'a [String],
        comparison: ComparisonContext<'a>,
    ) -> Self {
        Self {
            from_attrs_raw,
            from_attrs_projected,
            schema,
            defs,
            detail,
            leaf_provenance: DetailLeafProvenance::DecidedByPlan(changed_attribute_keys),
            forces_replacement_keys: None,
            cascade_ref_hints: &[],
            explicit: None,
            comparison,
        }
    }

    fn for_replace(
        from_attrs_raw: &'a CanonicalComparisonAttributes,
        from_attrs_projected: &'a CanonicalComparisonAttributes,
        schema: Option<&'a ResourceSchema>,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        detail: DetailLevel,
        replacement: ReplacementDiffContext<'a>,
        comparison: ComparisonContext<'a>,
    ) -> Self {
        Self {
            from_attrs_raw,
            from_attrs_projected,
            schema,
            defs,
            detail,
            leaf_provenance: DetailLeafProvenance::Recorded(replacement.nested_write_only),
            forces_replacement_keys: Some(replacement.forces_replacement_keys),
            cascade_ref_hints: replacement.cascade_ref_hints,
            explicit: None,
            comparison,
        }
    }

    fn with_explicit(mut self, explicit: Option<&'a ProjectedAuthoring>) -> Self {
        self.explicit = explicit;
        self
    }

    fn is_whole_value_authored(&self, key: &str) -> bool {
        is_whole_value(self.explicit_for_key(key))
    }

    fn payload_key_is_changed(&self, key: &str) -> bool {
        match self.leaf_provenance {
            DetailLeafProvenance::DecidedByPlan(keys) => keys.iter().any(|changed| changed == key),
            DetailLeafProvenance::Recorded(_) => self
                .forces_replacement_keys
                .is_some_and(|keys| keys.contains(key)),
        }
    }

    fn leaf_provenance(&self, key: &str) -> LeafProvenance<'_> {
        match self.leaf_provenance {
            DetailLeafProvenance::DecidedByPlan(keys) => LeafProvenance::DecidedByPlan {
                key_changed: keys.iter().any(|changed| changed == key),
            },
            DetailLeafProvenance::Recorded(record) => LeafProvenance::Recorded(record.get(key)),
        }
    }

    fn explicit_for_key(&self, key: &str) -> Option<&'a ProjectedAuthoring> {
        explicit_child(self.explicit, key)
    }
}

fn explicit_child<'a>(
    explicit: Option<&'a ProjectedAuthoring>,
    key: &str,
) -> Option<&'a ProjectedAuthoring> {
    match explicit {
        Some(ProjectedAuthoring::Struct { children }) => children.get(key),
        Some(
            ProjectedAuthoring::Opaque
            | ProjectedAuthoring::WholeValue
            | ProjectedAuthoring::LegacyList { .. }
            | ProjectedAuthoring::AlignedList { .. }
            | ProjectedAuthoring::UnalignedList,
        )
        | None => None,
    }
}

fn explicit_list_element(
    explicit: Option<&ProjectedAuthoring>,
    projected_index: usize,
) -> Option<&ProjectedAuthoring> {
    match explicit {
        Some(ProjectedAuthoring::AlignedList { elements }) => elements.get(projected_index),
        Some(ProjectedAuthoring::LegacyList { element }) => Some(element),
        Some(
            ProjectedAuthoring::Opaque
            | ProjectedAuthoring::Struct { .. }
            | ProjectedAuthoring::WholeValue
            | ProjectedAuthoring::UnalignedList,
        )
        | None => None,
    }
}

fn is_whole_value(explicit: Option<&ProjectedAuthoring>) -> bool {
    matches!(explicit, Some(ProjectedAuthoring::WholeValue))
}

fn project_from_attrs(
    from_attributes: &CanonicalComparisonAttributes,
    stored_attributes: Option<&CanonicalComparisonAttributes>,
    desired_attributes: &CanonicalComparisonAttributes,
    explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
    comparison: ComparisonContext<'_>,
) -> crate::explicit::ProjectedAttributes {
    // Project `from.attributes` through the user-authoring tree so
    // server-side default fields the user never wrote don't inflate
    // the unchanged-count summary (refs awscc#206). Stored values anchor
    // per-element list records when present; otherwise the projection uses
    // the legacy union fallback.
    let empty_stored = CanonicalComparisonAttributes::default();
    match explicit {
        Some(explicit) => ExplicitProjection::for_desired(
            stored_attributes.unwrap_or(&empty_stored),
            desired_attributes,
            explicit,
            schema,
            &comparison,
        )
        .project_attributes(from_attributes.clone()),
        None => crate::explicit::ProjectedAttributes::unprojected(from_attributes.clone(), None),
    }
}

fn project_previous_attrs(
    previous_attributes: &CanonicalComparisonAttributes,
    stored_attributes: Option<&CanonicalComparisonAttributes>,
    desired_attributes: &CanonicalComparisonAttributes,
    explicit: Option<&crate::explicit::ExplicitFields>,
    schema: Option<&ResourceSchema>,
    comparison: ComparisonContext<'_>,
) -> crate::explicit::ProjectedAttributes {
    // Same projection as `project_from_attrs`, but sourced from
    // replacement-display metadata instead of a replace effect state.
    project_from_attrs(
        previous_attributes,
        stored_attributes,
        desired_attributes,
        explicit,
        schema,
        comparison,
    )
}

fn build_attribute_diff_rows<S>(
    to: &crate::resource::Resource<S>,
    to_attributes: &CanonicalComparisonAttributes,
    ctx: AttributeDiffContext<'_>,
) -> AttributeDiffRows {
    let mut rows = Vec::new();

    let mut keys: Vec<_> = to_attributes.keys().cloned().collect();
    keys.extend(
        to.attributes
            .keys()
            .filter(|key| {
                !to_attributes.contains_key(key) && ctx.payload_key_is_changed(key.as_str())
            })
            .cloned(),
    );
    keys.sort();
    keys.dedup();

    // Attributes where `semantically_equal` reported a diff but the
    // per-shape builder produced no display rows. Two shapes today:
    // list-of-maps whose only diff was an upstream-injected key the IR
    // dropped (#2886), and Map whose only entries were empty nested
    // sections recursively suppressed (#2910). Counted into the
    // trailing unchanged-attributes summary so the final tally still
    // adds up.
    let mut effectively_unchanged: usize = 0;

    for key in keys {
        let payload_only = !to_attributes.contains_key(&key);
        let new_value = to_attributes
            .get(&key)
            .or_else(|| to.attributes.get(&key))
            .expect("display keys come from a desired value view");
        // Look up `old_value` from the projected map so MapDiff /
        // ListOfMapsDiff rows don't surface server-side default leaves
        // the user never authored (refs awscc#206). The same value the
        // unchanged-count computation below uses, kept consistent so a
        // field that's "unchanged after projection" is also "unchanged
        // for display".
        let forces_replacement = ctx
            .forces_replacement_keys
            .is_some_and(|keys| keys.contains(key.as_str()));
        let old_value = ctx.from_attrs_projected.get(&key).or_else(|| {
            // A forcing key can be absent from the authoring projection (for
            // example, a legacy replacement record). Fall back to raw only in
            // that case; when projection retained the key, keep its nested
            // server-added fields/elements filtered for replacement display.
            if forces_replacement {
                ctx.from_attrs_raw.get(&key)
            } else {
                None
            }
        });
        let attr_type = ctx
            .schema
            .and_then(|s| s.attributes.get(key.as_str()))
            .map(|a| &a.attr_type);
        // Keep the raw verdict only for the hidden-unchanged accounting below.
        let raw_is_same = old_value
            .map(|old| schema_aware_equal(old, new_value, attr_type, ctx.defs, &ctx.comparison))
            .unwrap_or(false);
        let type_info = attr_type.map(|attr_type| TypedAttr {
            attr_type,
            defs: ctx.defs,
        });
        let is_same = !key_should_enter_patch(
            &key,
            ctx.schema,
            ctx.leaf_provenance(&key),
            AttrComparison {
                from: old_value,
                to: new_value,
                saved: None,
                type_info,
            },
            &ctx.comparison,
        );

        if is_same {
            if !raw_is_same {
                effectively_unchanged += 1;
            }
            if forces_replacement {
                let old_str = old_value
                    .map(render_value)
                    .unwrap_or_else(|| render_other("(none)"));
                let new_str = ctx
                    .cascade_ref_hints
                    .iter()
                    .find(|(attr, _)| attr == &key)
                    .map(|(_, hint)| render_other(hint.clone()))
                    .unwrap_or_else(|| render_value(new_value));
                rows.push(DetailRow::ReplaceCascade {
                    key: key.to_string(),
                    old: old_str,
                    new: new_str,
                });
            }
            continue;
        }

        if payload_only {
            let old = old_value
                .map(render_value)
                .unwrap_or_else(|| render_other("(none)"));
            rows.push(changed_row(
                key,
                old,
                render_value(new_value),
                forces_replacement,
            ));
            continue;
        }

        // An attribute-level WholeValue means the entire value is authoritative.
        // This is the carried-list ambiguity fallback: using
        // element-level display pairing here could hide the pending removal
        // behind the same guessed identity the state record intentionally
        // refused to claim.
        if ctx.is_whole_value_authored(&key) && is_list_of_maps(new_value) {
            let old = old_value
                .map(render_value)
                .unwrap_or_else(|| render_other("(none)"));
            rows.push(changed_row(
                key.to_string(),
                old,
                render_value(new_value),
                forces_replacement,
            ));
            continue;
        }

        if is_list_of_maps(new_value) {
            match build_list_of_maps_diff_row(
                &key,
                old_value,
                new_value,
                attr_type,
                &ctx,
                forces_replacement,
            ) {
                Some(row) => rows.push(row),
                None if forces_replacement => rows.push(DetailRow::ForceReplaceListOfMapsHeader {
                    key: key.to_string(),
                }),
                None => effectively_unchanged += 1,
            }
        } else if should_render_as_map_diff(old_value, new_value) {
            match build_map_diff_row(
                &key,
                old_value,
                new_value,
                attr_type,
                ctx.defs,
                ctx.detail,
                forces_replacement,
                (key == "tags").then_some(&to.default_tag_keys),
                ctx.explicit_for_key(&key),
                ctx.comparison,
            ) {
                Some(row) => rows.push(row),
                None if forces_replacement => rows.push(DetailRow::ForceReplaceMapHeader {
                    key: key.to_string(),
                }),
                None => effectively_unchanged += 1,
            }
        } else if let Some(diff) =
            compute_string_list_change(old_value, new_value, attr_type, ctx.defs)
        {
            rows.push(string_list_diff_row(
                key.to_string(),
                diff.unchanged,
                diff.added,
                diff.removed,
                forces_replacement,
            ));
        } else {
            let old_str = old_value
                .map(render_value)
                .unwrap_or_else(|| render_other("(none)"));
            let new_str = render_value(new_value);
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
            // suppression would hide a real secret rotation.
            if !forces_replacement
                && old_str.plain() == new_str.plain()
                && old_value.is_none_or(|v| !crate::value::contains_secret(v))
                && !crate::value::contains_secret(new_value)
            {
                effectively_unchanged += 1;
                continue;
            }
            rows.push(changed_row(
                key.to_string(),
                old_str,
                new_str,
                forces_replacement,
            ));
        }
    }

    AttributeDiffRows {
        rows,
        effectively_unchanged,
    }
}

/// Build replacement detail rows for a decomposed Create/Delete replacement.
///
/// Phase 4 stores replacement display metadata separately from the Create
/// effect. This helper owns the delete-index lookup so every renderer consumes
/// the same masked historical view rather than reconstructing that lookup.
#[allow(clippy::too_many_arguments)]
pub fn build_replace_detail_rows_from_display<S>(
    plan: &Plan<S>,
    to: &crate::resource::Resource<S>,
    replacement: ReplaceDisplayInfo<'_>,
    historical_display: &HistoricalDisplayAttributes,
    registry: Option<&SchemaRegistry>,
    detail: DetailLevel,
    explicit: Option<&crate::explicit::ExplicitFields>,
    provider: &ProviderRouter,
) -> ProviderResult<Vec<DetailRow>> {
    let Some(Effect::Delete {
        id,
        identifier,
        generation,
        ..
    }) = plan.effects().get(replacement.delete_idx)
    else {
        return Ok(Vec::new());
    };
    let key = DeletedInstanceKey::new(
        id.as_inner().clone(),
        generation.clone(),
        identifier.to_string(),
    );
    let Some(historical_attributes) = historical_display.delete(&key) else {
        return Ok(Vec::new());
    };
    let registry = registry
        .ok_or_else(|| ComparisonResolutionError::MissingResourceSchema { id: to.id.clone() })?;
    let resolved = provider.comparison_for(registry, &to.id)?;
    let schema = resolved.owner_schema().schema().as_schema();
    let comparison_attributes =
        CanonicalComparisonAttributes::canonicalize(to.resolved_attributes(), schema);
    Ok(build_replace_rows_from_attributes(
        &historical_attributes.attributes,
        historical_attributes.stored_attributes.as_ref(),
        &historical_attributes.nested_write_only,
        to,
        &comparison_attributes,
        replacement.changed_create_only,
        replacement.temporary_name,
        replacement.cascade_ref_hints,
        Some(schema),
        detail,
        explicit,
        *resolved.context(),
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_replace_rows_from_attributes<S>(
    previous_attributes: &HashMap<String, Value>,
    stored_attributes: Option<&HashMap<String, Value>>,
    nested_write_only: &NestedWriteOnlyRecord,
    to: &crate::resource::Resource<S>,
    comparison_attributes: &CanonicalComparisonAttributes,
    changed_create_only: &[String],
    temporary_name: Option<&crate::effect::TemporaryName>,
    cascade_ref_hints: &[(String, String)],
    schema: Option<&ResourceSchema>,
    detail: DetailLevel,
    explicit: Option<&crate::explicit::ExplicitFields>,
    comparison: ComparisonContext<'_>,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();
    let defs = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());
    let previous_attributes = match schema {
        Some(schema) => {
            CanonicalComparisonAttributes::canonicalize(previous_attributes.clone(), schema)
        }
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(previous_attributes.clone()),
        ),
    };
    let stored_attributes = stored_attributes.cloned().map(|attributes| match schema {
        Some(schema) => CanonicalComparisonAttributes::canonicalize(attributes, schema),
        None => CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(attributes),
        ),
    });
    let projected = project_previous_attrs(
        &previous_attributes,
        stored_attributes.as_ref(),
        comparison_attributes,
        explicit,
        schema,
        comparison,
    );
    let from_attrs_projected = projected.attributes();
    let force_keys: HashSet<&str> = changed_create_only.iter().map(|s| s.as_str()).collect();

    let attr_rows = build_attribute_diff_rows(
        to,
        comparison_attributes,
        AttributeDiffContext::for_replace(
            &previous_attributes,
            from_attrs_projected,
            schema,
            defs,
            detail,
            ReplacementDiffContext {
                nested_write_only,
                forces_replacement_keys: &force_keys,
                cascade_ref_hints,
            },
            comparison,
        )
        .with_explicit(Some(&projected.authoring)),
    );
    rows.extend(attr_rows.rows);

    // Show removed create-only attributes.
    let mut removed_force_keys: Vec<_> = changed_create_only
        .iter()
        .filter(|key| !to.attributes.contains_key(key.as_str()))
        .collect();
    removed_force_keys.sort();
    for key in removed_force_keys {
        let old_value = previous_attributes.get(key.as_str());
        let attr_type = schema
            .and_then(|s| s.attributes.get(key.as_str()))
            .map(|a| &a.attr_type);
        rows.push(build_replace_removed_row(
            key, old_value, attr_type, defs, detail, comparison,
        ));
    }

    let mut removed_non_force_keys: Vec<_> = from_attrs_projected
        .keys()
        .filter(|key| {
            !to.attributes.contains_key(key.as_str()) && !force_keys.contains(key.as_str())
        })
        .collect();
    removed_non_force_keys.sort();
    for key in removed_non_force_keys {
        if let Some(old_value) = from_attrs_projected.get(key.as_str()) {
            let attr_type = schema
                .and_then(|s| s.attributes.get(key.as_str()))
                .map(|a| &a.attr_type);
            rows.push(build_removed_attr_row(
                key, old_value, attr_type, defs, detail, comparison,
            ));
        }
    }

    // Temporary name note
    if let Some(temp) = temporary_name {
        rows.push(DetailRow::TemporaryNameNote {
            can_rename: temp.can_rename,
            temporary_value: render_string_text(&temp.temporary_value),
            original_value: render_string_text(&temp.original_value),
            attribute: temp.attribute.clone(),
        });
    }

    // In Full mode, show count of unchanged attributes hidden.
    if detail == DetailLevel::Full {
        let unchanged_count = compute_unchanged_count(
            from_attrs_projected,
            comparison_attributes,
            Some(&force_keys),
            schema,
            &comparison,
        ) + attr_rows.effectively_unchanged;
        if unchanged_count > 0 {
            rows.push(DetailRow::HiddenUnchanged {
                count: unchanged_count,
            });
        }
    }

    rows
}

fn build_replace_removed_row(
    key: &str,
    old_value: Option<&Value>,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    comparison: ComparisonContext<'_>,
) -> DetailRow {
    let Some(old_value) = old_value else {
        return DetailRow::ReplaceRemoved {
            key: key.to_string(),
            old: render_other("(none)"),
        };
    };

    if let Some(DetailRow::MapDiff { entries, .. }) = build_map_removed_row(key, old_value) {
        return DetailRow::MapDiffForcesReplacement {
            key: key.to_string(),
            entries,
        };
    }

    if is_list_of_maps(old_value) {
        let empty_list = Value::Concrete(ConcreteValue::List(Vec::new()));
        let (unchanged, modified, added, removed) = compute_list_of_maps_diff_parts(
            Some(old_value),
            &empty_list,
            attr_type,
            defs,
            detail,
            comparison,
        );
        if let Some(block) =
            NonEmptyListOfMapsBlock::from_parts(unchanged, modified, added, removed)
        {
            return DetailRow::ListOfMapsDiffForcesReplacement {
                key: key.to_string(),
                block,
            };
        }
    }

    DetailRow::ReplaceRemoved {
        key: key.to_string(),
        old: render_value(old_value),
    }
}

fn build_removed_attr_row(
    key: &str,
    old_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    comparison: ComparisonContext<'_>,
) -> DetailRow {
    if let Some(row) = build_map_removed_row(key, old_value) {
        return row;
    }

    if is_list_of_maps(old_value) {
        let empty_list = Value::Concrete(ConcreteValue::List(Vec::new()));
        let (unchanged, modified, added, removed) = compute_list_of_maps_diff_parts(
            Some(old_value),
            &empty_list,
            attr_type,
            defs,
            detail,
            comparison,
        );
        if let Some(block) =
            NonEmptyListOfMapsBlock::from_parts(unchanged, modified, added, removed)
        {
            return DetailRow::ListOfMapsDiff {
                key: key.to_string(),
                block,
            };
        }
    }

    DetailRow::Removed {
        key: key.to_string(),
        old: render_value(old_value),
    }
}

fn build_delete_rows(
    id: &ResourceId,
    identifier: &str,
    generation: &EffectGeneration,
    historical_attributes: &HistoricalDisplayAttributes,
) -> Vec<DetailRow> {
    let mut rows = Vec::new();

    let key = DeletedInstanceKey::new(id.clone(), generation.clone(), identifier.to_string());
    if let Some(historical) = historical_attributes.delete(&key) {
        let attrs = &historical.attributes;
        let mut keys: Vec<_> = attrs.keys().collect();
        keys.sort();
        for key in keys {
            let value = &attrs[key];
            // Route lists through PrettyAttribute so `PrettyValue::render`
            // applies its 80-col threshold and YAML-style vertical layout,
            // mirroring `build_create_rows`. Without this, a list-of-maps
            // like `domain_validation_options` renders on one long,
            // unreadable line (the Delete path was asymmetric with Create).
            if let Value::Concrete(ConcreteValue::List(_)) = value {
                rows.push(DetailRow::PrettyAttribute {
                    key: key.to_string(),
                    value: PrettyValue::new(value.clone()),
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
                    value: render_value(value),
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
            key: RenderedKey::from_raw(k),
            value: render_value(&old_map[k]),
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
/// When `default_tag_keys` is `Some`, top-level `Added` and `Changed` entries
/// whose key is in the set receive the `# default_tags` annotation; `Removed`
/// and nested entries never do.
#[allow(clippy::too_many_arguments)]
fn build_map_diff_row(
    key: &str,
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    forces_replacement: bool,
    default_tag_keys: Option<&DefaultTagKeys>,
    explicit: Option<&ProjectedAuthoring>,
    comparison: ComparisonContext<'_>,
) -> Option<DetailRow> {
    let mut entries = compute_map_diff_entries_with_explicit(
        old_value, new_value, attr_type, defs, detail, explicit, comparison,
    );
    if let Some(default_tag_keys) = default_tag_keys {
        for entry in &mut entries {
            match entry {
                MapDiffEntryIR::Added {
                    key, annotation, ..
                }
                | MapDiffEntryIR::Changed {
                    key, annotation, ..
                } => {
                    *annotation = default_tag_annotation(default_tag_keys, key.raw());
                }
                MapDiffEntryIR::Removed { .. }
                | MapDiffEntryIR::NestedMapDiff { .. }
                | MapDiffEntryIR::NestedListOfMapsDiff { .. }
                | MapDiffEntryIR::StringListChanged { .. } => {}
            }
        }
    }
    let entries = NonEmptyVec::from_vec(entries)?;
    Some(map_diff_row(key.to_string(), entries, forces_replacement))
}

#[cfg(test)]
fn compute_map_diff_entries(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    comparison: ComparisonContext<'_>,
) -> Vec<MapDiffEntryIR> {
    compute_map_diff_entries_with_explicit(
        old_value, new_value, attr_type, defs, detail, None, comparison,
    )
}

fn compute_map_diff_entries_with_explicit(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    explicit: Option<&ProjectedAuthoring>,
    comparison: ComparisonContext<'_>,
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
                    key: RenderedKey::from_raw(&entry.key),
                    value: render_value(&entry.value),
                    annotation: None,
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
                let entry_explicit = explicit_child(explicit, &e.key);
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
                    && schema_aware_equal(&e.old_value, &e.new_value, entry_type, defs, &comparison)
                {
                    continue;
                }
                if is_whole_value(entry_explicit) && is_list_of_maps(&e.new_value) {
                    entries.push(MapDiffEntryIR::Changed {
                        key: RenderedKey::from_raw(&e.key),
                        old: render_value(&e.old_value),
                        new: render_value(&e.new_value),
                        annotation: None,
                    });
                    continue;
                }
                // If both old and new are maps, recursively diff
                if matches!(&e.old_value, Value::Concrete(ConcreteValue::Map(_)))
                    && matches!(&e.new_value, Value::Concrete(ConcreteValue::Map(_)))
                {
                    let nested = compute_map_diff_entries_with_explicit(
                        Some(&e.old_value),
                        &e.new_value,
                        entry_type,
                        defs,
                        detail,
                        entry_explicit,
                        comparison,
                    );
                    // #2910: `from_vec` returns None for the all-empty
                    // recursive case (every grandchild was itself
                    // suppressed). Skip pushing so the parent header
                    // never renders empty.
                    if let Some(entries_nev) = NonEmptyVec::from_vec(nested) {
                        entries.push(MapDiffEntryIR::NestedMapDiff {
                            key: RenderedKey::from_raw(&e.key),
                            entries: entries_nev,
                        });
                    }
                } else if is_list_of_maps(&e.new_value) {
                    // List-of-maps: compute per-item field-level diffs
                    let (_, modified, added, removed) =
                        compute_list_of_maps_diff_parts_with_explicit(
                            Some(&e.old_value),
                            &e.new_value,
                            entry_type,
                            defs,
                            detail,
                            entry_explicit,
                            comparison,
                        );
                    // #2910: `from_parts` returns None when every
                    // paired element was dropped per #2886 and there
                    // are no wholly-added / wholly-removed elements
                    // either.
                    if let Some(block) =
                        NonEmptyListOfMapsBlock::from_parts(Vec::new(), modified, added, removed)
                    {
                        entries.push(MapDiffEntryIR::NestedListOfMapsDiff {
                            key: RenderedKey::from_raw(&e.key),
                            block,
                        });
                    }
                } else if let Some(diff) =
                    compute_string_list_change(Some(&e.old_value), &e.new_value, entry_type, defs)
                {
                    // #3234: see MapDiffEntryIR::StringListChanged docs.
                    entries.push(MapDiffEntryIR::StringListChanged {
                        key: RenderedKey::from_raw(&e.key),
                        unchanged: diff.unchanged,
                        added: diff.added,
                        removed: diff.removed,
                    });
                } else {
                    let old_s = render_value(&e.old_value);
                    let new_s = render_value(&e.new_value);
                    // carina#3258: see the renderer-truth note in
                    // `build_update_rows` (including the secret
                    // bypass). Map-level hidden counts roll up at the
                    // attribute level, so silently dropping the entry
                    // (matching the schema-aware leaf-skip arm above)
                    // is the correct behavior here.
                    if old_s.plain() == new_s.plain()
                        && !crate::value::contains_secret(&e.old_value)
                        && !crate::value::contains_secret(&e.new_value)
                    {
                        continue;
                    }
                    entries.push(MapDiffEntryIR::Changed {
                        key: RenderedKey::from_raw(&e.key),
                        old: old_s,
                        new: new_s,
                        annotation: None,
                    });
                }
            }
            crate::diff_helpers::MapDiffItem::Added(e) => {
                entries.push(MapDiffEntryIR::Added {
                    key: RenderedKey::from_raw(&e.key),
                    value: render_value(&e.value),
                    annotation: None,
                });
            }
            crate::diff_helpers::MapDiffItem::Removed(e) => {
                entries.push(MapDiffEntryIR::Removed {
                    key: RenderedKey::from_raw(&e.key),
                    value: render_value(&e.value),
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
    ctx: &AttributeDiffContext<'_>,
    forces_replacement: bool,
) -> Option<DetailRow> {
    let (unchanged, modified, added, removed) = compute_list_of_maps_diff_parts_with_explicit(
        old_value,
        new_value,
        attr_type,
        ctx.defs,
        ctx.detail,
        ctx.explicit_for_key(key),
        ctx.comparison,
    );
    let block = NonEmptyListOfMapsBlock::from_parts(unchanged, modified, added, removed)?;
    Some(list_of_maps_diff_row(
        key.to_string(),
        block,
        forces_replacement,
    ))
}

fn compute_list_of_maps_diff_parts(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    comparison: ComparisonContext<'_>,
) -> (
    Vec<RenderedValue>,
    Vec<ListOfMapsDiffModified>,
    Vec<ListOfMapsDiffItem>,
    Vec<ListOfMapsDiffItem>,
) {
    compute_list_of_maps_diff_parts_with_explicit(
        old_value, new_value, attr_type, defs, detail, None, comparison,
    )
}

fn compute_list_of_maps_diff_parts_with_explicit(
    old_value: Option<&Value>,
    new_value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    detail: DetailLevel,
    explicit: Option<&ProjectedAuthoring>,
    comparison: ComparisonContext<'_>,
) -> (
    Vec<RenderedValue>,
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
    // `Struct`); shared with differ/writeback list walks so Ref peeling
    // and already-at-element call sites cannot drift.
    let item_type = list_element_type(attr_type, defs);

    let mut old_matched = vec![false; old_items.len()];
    let mut new_matched = vec![false; new_items.len()];

    // Phase 1: Find exact matches. Site 3 (carina#3073): schema-aware
    // so two statements differing only by enum spelling
    // (`effect: allow` vs `Allow`) match as identical instead of
    // being reported as removed+added.
    for (ni, new_item) in new_items.iter().enumerate() {
        for (oi, old_item) in old_items.iter().enumerate() {
            if !old_matched[oi]
                && schema_aware_equal(old_item, new_item, item_type, defs, &comparison)
            {
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

    // Phase 2: pair unmatched items through the same schema-aware,
    // best-first routine used by differ and writeback.
    let mut paired: Vec<(usize, usize)> = Vec::new();
    let mut paired_old = vec![false; unmatched_old.len()];
    let mut paired_new = vec![false; unmatched_new.len()];

    let unmatched_new_items: Vec<&Value> = unmatched_new
        .iter()
        .map(|&index| &new_items[index])
        .collect();
    let unmatched_old_items: Vec<&Value> = unmatched_old
        .iter()
        .map(|&index| &old_items[index])
        .collect();
    for (ui_new, ui_old) in pair_list_elements(
        &unmatched_new_items,
        &unmatched_old_items,
        item_type,
        defs,
        &comparison,
    )
    .into_iter()
    .enumerate()
    {
        if let Some(ui_old) = ui_old {
            paired.push((unmatched_old[ui_old], unmatched_new[ui_new]));
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
        if matches!(new_item, Value::Concrete(ConcreteValue::Map(_))) && new_matched[ni] {
            unchanged.push(render_value(new_item));
        }
    }

    let mut modified = Vec::new();
    for &(oi, ni) in &paired {
        if let (
            Value::Concrete(ConcreteValue::Map(old_map)),
            Value::Concrete(ConcreteValue::Map(new_map)),
        ) = (&old_items[oi], &new_items[ni])
        {
            let element_explicit = explicit_list_element(explicit, oi);
            let mut keys: Vec<_> = new_map.keys().collect();
            keys.sort();
            let mut fields: Vec<ListOfMapsDiffField> = Vec::new();
            let mut unchanged_count: usize = 0;
            for k in keys {
                let field_explicit = explicit_child(element_explicit, k);
                // Site 4 (carina#3073): resolve the struct field's
                // type so an enum-equal field (`effect: allow` vs
                // `Allow`) is treated as unchanged instead of emitting
                // a phantom `~ effect` sub-row under a sibling-changed
                // statement.
                let field_type = map_entry_subtype(item_type, k, defs);
                let field_same = old_map
                    .get(k)
                    .map(|old| schema_aware_equal(old, &new_map[k], field_type, defs, &comparison))
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
                    let nested = compute_map_diff_entries_with_explicit(
                        old_val,
                        &new_map[k],
                        field_type,
                        defs,
                        detail,
                        field_explicit,
                        comparison,
                    );
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
                        key: RenderedKey::from_raw(k),
                        entries: nested,
                    });
                } else if is_whole_value(field_explicit) && is_list_of_maps(&new_map[k]) {
                    fields.push(ListOfMapsDiffField::Changed {
                        key: RenderedKey::from_raw(k),
                        old: old_val
                            .map(render_value)
                            .unwrap_or_else(|| render_other("(none)")),
                        new: render_value(&new_map[k]),
                    });
                } else if let Some(diff) =
                    compute_string_list_change(old_val, &new_map[k], field_type, defs)
                {
                    fields.push(ListOfMapsDiffField::StringListChanged {
                        key: RenderedKey::from_raw(k),
                        unchanged: diff.unchanged,
                        added: diff.added,
                        removed: diff.removed,
                    });
                } else {
                    let old_v = old_val
                        .map(render_value)
                        .unwrap_or_else(|| render_other("(none)"));
                    let new_v = render_value(&new_map[k]);
                    // carina#3258: see the renderer-truth note in
                    // `build_update_rows` (including the secret
                    // bypass). Fold display-equal peers into the
                    // per-element hidden-unchanged count so the
                    // summary stays consistent with the rendered
                    // evidence.
                    if old_v.plain() == new_v.plain()
                        && old_val.is_none_or(|v| !crate::value::contains_secret(v))
                        && !crate::value::contains_secret(&new_map[k])
                    {
                        unchanged_count += 1;
                        continue;
                    }
                    fields.push(ListOfMapsDiffField::Changed {
                        key: RenderedKey::from_raw(k),
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
                    .map(|(k, v)| (RenderedKey::from_raw(k), PrettyValue::new(v.clone())))
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
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Option<crate::diff_helpers::StringListDiff<RenderedValue>> {
    let enum_context = attr_type
        .and_then(|attr_type| string_list_inner_type(attr_type, defs))
        .and_then(AttributeType::enum_parts)
        .map(|(_, values, _, _, dsl_map)| EnumListDiffContext {
            valid_values: values.into_iter().flatten().map(String::as_str).collect(),
            dsl_map,
        });

    let new_list = project_string_list_diff_elements(new_value, enum_context.as_ref())?;
    let old_list = match old_value {
        Some(old_value) => project_string_list_diff_elements(old_value, enum_context.as_ref())?,
        None => Vec::new(),
    };
    // carina#3075: when the element type is a `Enum`, the set-diff
    // must compare enum *values*, not raw spellings — otherwise a
    // DSL-alias element (`allow`) vs its API-canonical form (`Allow`) is
    // reported as a phantom `- allow / + Allow` pair alongside any genuine
    // add/remove. Canonicalize only typed enum values to the API spelling.
    // Ordinary strings remain literal comparison keys (including dotted
    // prefixes), while the enum-list equality rule still compares keys
    // case-insensitively.
    let enum_typed = enum_context.is_some();
    let diff = crate::diff_helpers::compute_string_list_diff_by(
        &old_list,
        &new_list,
        |left, right| {
            if enum_typed {
                crate::resource::enum_values_equal(&left.comparison_key, &right.comparison_key)
            } else {
                left.comparison_key == right.comparison_key
            }
        },
        |element| element.display.clone(),
    );
    if diff.added.is_empty() && diff.removed.is_empty() {
        return None;
    }
    Some(diff)
}

struct StringListDiffElement {
    comparison_key: String,
    display: RenderedValue,
}

struct EnumListDiffContext<'a> {
    valid_values: Vec<&'a str>,
    dsl_map: crate::schema::DslMap<'a>,
}

/// Project a list-like value exactly once, retaining both the comparison key
/// and the display text from the same source element. Only typed enum values
/// are canonicalized; strings stay strings even when the schema's element
/// type is an enum.
fn project_string_list_diff_elements(
    value: &Value,
    enum_context: Option<&EnumListDiffContext<'_>>,
) -> Option<Vec<StringListDiffElement>> {
    match value {
        Value::Concrete(ConcreteValue::StringList(items)) => Some(
            items
                .iter()
                .map(|item| {
                    let plaintext = item.expose_plaintext();
                    StringListDiffElement {
                        comparison_key: plaintext.to_string(),
                        display: render_string_text(plaintext),
                    }
                })
                .collect(),
        ),
        Value::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .map(|item| match item {
                Value::Concrete(ConcreteValue::String(text)) => {
                    let plaintext = text.expose_plaintext();
                    Some(StringListDiffElement {
                        comparison_key: plaintext.to_string(),
                        display: render_string_text(plaintext),
                    })
                }
                Value::Concrete(ConcreteValue::EnumIdentifier(identifier)) => {
                    let comparison_key = enum_context.map_or_else(
                        || identifier.as_str().to_string(),
                        |context| {
                            identifier
                                .canonical_api_spelling(&context.valid_values, &context.dsl_map)
                        },
                    );
                    Some(StringListDiffElement {
                        display: render_enum_text(comparison_key.clone()),
                        comparison_key,
                    })
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                    let comparison_key = value.api_value().to_string();
                    Some(StringListDiffElement {
                        display: render_enum_text(comparison_key.clone()),
                        comparison_key,
                    })
                }
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Return a `List`'s element type, descending recursively through the lazy,
/// definition-aware Union-member view. A visited-node set terminates cyclic
/// Union aliases without hiding a reachable sibling List arm.
fn string_list_inner_type<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    fn on_path<'a>(
        attr_type: &'a AttributeType,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        visited: &mut std::collections::HashSet<*const AttributeType>,
    ) -> Option<&'a AttributeType> {
        let resolved = attr_type.resolve_refs_with_defs(defs).as_attr();
        let identity = std::ptr::from_ref(resolved);
        if !visited.insert(identity) {
            return None;
        }
        let result = match resolved.shape_ref_free().ok()? {
            crate::schema::Shape::List {
                element_type: inner,
                ..
            } => Some(inner),
            crate::schema::Shape::Union => crate::schema::union_members_with_defs(resolved, defs)?
                .iter()
                .flatten()
                .find_map(|member| on_path(member.as_attr(), defs, visited)),
            crate::schema::Shape::String { .. }
            | crate::schema::Shape::Int { .. }
            | crate::schema::Shape::Float { .. }
            | crate::schema::Shape::Bool
            | crate::schema::Shape::Duration
            | crate::schema::Shape::Enum { .. }
            | crate::schema::Shape::Map { .. }
            | crate::schema::Shape::Struct { .. } => None,
        };
        visited.remove(&identity);
        result
    }

    on_path(attr_type, defs, &mut std::collections::HashSet::new())
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;
    use crate::effect::{ChangedCreateOnly, EffectGeneration};
    use crate::explicit::{ExplicitFields, PendingPaths};
    use crate::parser::ProviderConfig;
    use crate::plan::{ReplaceDisplayMetadata, SavedPlan, UnresolvedSavedPlan};
    use crate::resource::{
        ExpandedConcreteValue as ConcreteValue, ExpandedResource as Resource,
        ExpandedValue as Value, ResolvedResource, ResourceId, State,
    };
    use std::collections::HashSet;

    fn build_replace_detail_rows_from_display<S>(
        plan: &Plan<S>,
        to: &crate::resource::Resource<S>,
        replacement: ReplaceDisplayInfo<'_>,
        historical_display: &HistoricalDisplayAttributes,
        registry: Option<&SchemaRegistry>,
        detail: DetailLevel,
        explicit: Option<&ExplicitFields>,
    ) -> Vec<DetailRow> {
        let base_registry = registry.cloned().unwrap_or_default();
        let (registry, provider) =
            crate::provider::comparison_fixture_for_test(&base_registry, std::iter::once(&to.id));
        super::build_replace_detail_rows_from_display(
            plan,
            to,
            replacement,
            historical_display,
            Some(&registry),
            detail,
            explicit,
            &provider,
        )
        .expect("test replacement display must resolve its comparison")
    }

    fn compute_map_diff_entries(
        old_value: Option<&Value>,
        new_value: &Value,
        attr_type: Option<&AttributeType>,
        defs: &std::collections::BTreeMap<String, AttributeType>,
        detail: DetailLevel,
    ) -> Vec<MapDiffEntryIR> {
        super::compute_map_diff_entries(
            old_value,
            new_value,
            attr_type,
            defs,
            detail,
            crate::differ::exact_comparison_context_for_tests(),
        )
    }

    fn compute_list_of_maps_diff_parts(
        old_value: Option<&Value>,
        new_value: &Value,
        attr_type: Option<&AttributeType>,
        defs: &std::collections::BTreeMap<String, AttributeType>,
        detail: DetailLevel,
    ) -> (
        Vec<RenderedValue>,
        Vec<ListOfMapsDiffModified>,
        Vec<ListOfMapsDiffItem>,
        Vec<ListOfMapsDiffItem>,
    ) {
        super::compute_list_of_maps_diff_parts(
            old_value,
            new_value,
            attr_type,
            defs,
            detail,
            crate::differ::exact_comparison_context_for_tests(),
        )
    }

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn rendered_key(key: &str) -> RenderedKey {
        RenderedKey::from_raw(key)
    }

    fn pretty_value(value: Value) -> PrettyValue {
        PrettyValue::new(value)
    }

    fn build_detail_rows(
        effect: &Effect,
        registry: Option<&SchemaRegistry>,
        detail: DetailLevel,
        delete_attributes: Option<&HashMap<ResourceId, HashMap<String, Value>>>,
        prev_explicit: Option<&HashMap<ResourceId, crate::explicit::ExplicitFields>>,
    ) -> Vec<DetailRow> {
        let mut plan = Plan::new();
        plan.add(effect.clone());
        let mut historical_attributes = HistoricalDisplayAttributes::for_plan(&plan);
        if let Effect::Update { from, .. } = effect
            && let Some(update) = historical_attributes.updates.get_mut(&from.id)
        {
            update.stored_attributes = Some(from.attributes.clone());
        }
        let empty_nested_write_only = NestedWriteOnlyRecord::new();
        if let Some(delete_attributes) = delete_attributes {
            for key in effect.deleted_resource_attribute_keys() {
                if let Some(attributes) = delete_attributes.get(key.id()) {
                    historical_attributes.set_delete_from_provider_and_persisted(
                        key,
                        attributes,
                        None,
                        &empty_nested_write_only,
                    );
                }
            }
        }
        let base_registry = registry.cloned().unwrap_or_default();
        let (registry, provider) = crate::provider::comparison_fixture_for_test(
            &base_registry,
            std::iter::once(effect.resource_id()),
        );
        build_detail_rows_by_instance(
            effect,
            Some(&registry),
            detail,
            &historical_attributes,
            prev_explicit,
            &provider,
        )
        .expect("test detail display must resolve its comparison")
    }

    #[test]
    fn saved_plan_secret_rehydration_renders_from_rehydrated_effect() {
        let secret = |plaintext: &str| {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.into()),
            ))))
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "",
            PatternResourceSchema::new("test.Widget")
                .attribute(
                    PatternAttributeSchema::optional("password", PatternAttributeType::string())
                        .build(),
                )
                .validate()
                .expect("secret fixture schema must be valid"),
        );
        let desired = Resource::new("test.Widget", "example")
            .with_attribute("password", secret("new-secret"));
        let id = desired.id.clone();
        let to = ResolvedResource::resolve_with_schemas(desired.clone(), &registry)
            .expect("desired fixture resolves with its schema");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("old-secret".into())),
            )]),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(from),
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            to,
            changed_attributes: vec!["password".to_string()],
        });

        let redacted =
            crate::value::redact_secrets_in_plan(&plan).expect("saved-plan fixture secrets redact");
        let wire =
            serde_json::to_string(&SavedPlan::from(redacted)).expect("saved plan serializes");
        let unresolved: UnresolvedSavedPlan =
            serde_json::from_str(&wire).expect("saved plan wire format deserializes");
        let loaded = unresolved
            .resolve_with_schemas(&registry)
            .expect("saved plan resolves with current schemas");

        let deferred_templates: crate::parser::ResolvedDeferredForExpressions =
            crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test([]);
        let rehydrated = crate::value::rehydrate_plan_desired_secrets(
            &loaded,
            &HashMap::from([(id.clone(), desired)]),
            &HashMap::new(),
            &HashMap::new(),
            &deferred_templates,
        )
        .expect("fresh desired secret rehydrates the saved plan");
        let Effect::Update { to, .. } = &rehydrated.effects()[0] else {
            panic!("fixture must remain an Update effect");
        };
        assert!(matches!(
            to.attributes.get("password"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));

        let historical = HistoricalDisplayAttributes::for_plan(&rehydrated);
        let (registry, provider) =
            crate::provider::comparison_fixture_for_test(&registry, std::iter::once(&id));
        let rows = build_detail_rows_by_instance(
            &rehydrated.effects()[0],
            Some(&registry),
            DetailLevel::Explicit,
            &historical,
            None,
            &provider,
        )
        .expect("rehydrated update rows render");
        assert!(rows.iter().any(|row| matches!(
            row,
            DetailRow::Changed { key, .. } if key == "password"
        )));
    }

    fn replace_rows_for_test(
        from: State,
        to: Resource,
        changed_create_only: &[&str],
        detail: DetailLevel,
        cascade_ref_hints: Vec<(String, String)>,
        prev_explicit: Option<&HashMap<ResourceId, crate::explicit::ExplicitFields>>,
    ) -> Vec<DetailRow> {
        let id = to.id.clone();
        let changed_create_only: Vec<String> = changed_create_only
            .iter()
            .map(|attr| (*attr).to_string())
            .collect();
        let comparison_attributes = CanonicalComparisonAttributes::from_unschematized(
            crate::resource::UnschematizedComparisonAttributes::new(to.resolved_attributes()),
        );
        build_replace_rows_from_attributes(
            &from.attributes,
            Some(&from.attributes),
            &NestedWriteOnlyRecord::new(),
            &to,
            &comparison_attributes,
            &changed_create_only,
            None,
            &cascade_ref_hints,
            None,
            detail,
            prev_explicit.and_then(|map| map.get(&id)),
            crate::differ::exact_comparison_context_for_tests(),
        )
    }

    fn empty_struct_comparison_registry() -> SchemaRegistry {
        let config = PatternAttributeType::struct_(
            "Config",
            vec![PatternStructField::optional("label", PatternAttributeType::string()).build()],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "",
            PatternResourceSchema::new("test.Widget")
                .attribute(
                    PatternAttributeSchema::optional("name", PatternAttributeType::string())
                        .build(),
                )
                .attribute(PatternAttributeSchema::optional("config", config.clone()).build())
                .attribute(
                    PatternAttributeSchema::optional(
                        "settings",
                        PatternAttributeType::struct_(
                            "Settings",
                            vec![
                                PatternStructField::optional("x", PatternAttributeType::int())
                                    .build(),
                                PatternStructField::optional("config", config).build(),
                            ],
                        ),
                    )
                    .build(),
                )
                .validate()
                .expect("schema fixture must be valid"),
        );
        registry
    }

    #[test]
    fn update_rows_do_not_render_top_level_absent_equivalent_struct() {
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id,
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            )]),
        );
        let to = Resource::new("test.Widget", "example")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("after".into())),
            )
            .with_attribute(
                "config",
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["name".to_string()],
        };

        let rows = build_detail_rows(
            &effect,
            Some(&empty_struct_comparison_registry()),
            DetailLevel::Full,
            None,
            None,
        );

        assert!(matches!(
            rows.as_slice(),
            [DetailRow::Changed { key, .. }] if key == "name"
        ));
    }

    #[test]
    fn update_rows_do_not_render_nested_absent_equivalent_struct() {
        let settings = |include_config| {
            let mut fields =
                IndexMap::from([("x".to_string(), Value::Concrete(ConcreteValue::Int(1)))]);
            if include_config {
                fields.insert(
                    "config".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id,
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("before".into())),
                ),
                ("settings".to_string(), settings(false)),
            ]),
        );
        let to = Resource::new("test.Widget", "example")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("after".into())),
            )
            .with_attribute("settings", settings(true));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["name".to_string()],
        };

        let rows = build_detail_rows(
            &effect,
            Some(&empty_struct_comparison_registry()),
            DetailLevel::Full,
            None,
            None,
        );

        assert!(matches!(
            rows.as_slice(),
            [
                DetailRow::Changed { key, .. },
                DetailRow::HiddenUnchanged { count: 1 }
            ] if key == "name"
        ));
    }

    #[test]
    fn update_rows_recanonicalize_parent_after_masking_ignored_missing_leaf() {
        let settings = |label: Option<&str>| {
            let mut fields =
                IndexMap::from([("x".to_string(), Value::Concrete(ConcreteValue::Int(1)))]);
            if let Some(label) = label {
                fields.insert(
                    "config".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "label".to_string(),
                        Value::Concrete(ConcreteValue::String(label.into())),
                    )]))),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let registry = empty_struct_comparison_registry();
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id,
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("before".into())),
                ),
                ("settings".to_string(), settings(None)),
            ]),
        );
        let mut to = Resource::new("test.Widget", "example")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("after".into())),
            )
            .with_attribute("settings", settings(Some("configured")));
        to.directives.ignore_changes = vec![crate::resource::ParsedIgnorePath::new(vec![
            crate::resource::ParsedIgnorePathSegment::Attribute("settings".to_string()),
            crate::resource::ParsedIgnorePathSegment::Attribute("config".to_string()),
            crate::resource::ParsedIgnorePathSegment::Attribute("label".to_string()),
        ])];
        let to = ResolvedResource::resolve_with_schemas(to, &registry)
            .expect("ignore_changes path must resolve");
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from),
            to,
            changed_attributes: vec!["name".to_string()],
        };

        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);

        assert!(matches!(
            rows.as_slice(),
            [DetailRow::Changed { key, .. }] if key == "name"
        ));
    }

    #[test]
    fn update_rows_canonicalize_persisted_list_elements_before_projection() {
        let rule = |include_empty_options| {
            let mut fields =
                IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
            if include_empty_options {
                fields.insert(
                    "opts".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let rules = |include_empty_options| {
            Value::Concrete(ConcreteValue::List(vec![rule(include_empty_options)]))
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "",
            PatternResourceSchema::new("test.Widget")
                .attribute(
                    PatternAttributeSchema::optional("name", PatternAttributeType::string())
                        .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "rules",
                        PatternAttributeType::list(PatternAttributeType::struct_(
                            "Rule",
                            vec![
                                PatternStructField::required("port", PatternAttributeType::int())
                                    .build(),
                                PatternStructField::optional(
                                    "opts",
                                    PatternAttributeType::struct_(
                                        "Options",
                                        vec![
                                            PatternStructField::optional(
                                                "label",
                                                PatternAttributeType::string(),
                                            )
                                            .build(),
                                        ],
                                    ),
                                )
                                .build(),
                            ],
                        )),
                    )
                    .build(),
                )
                .validate()
                .expect("schema fixture must be valid"),
        );
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id.clone(),
            HashMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("before".into())),
                ),
                ("rules".to_string(), rules(false)),
            ]),
        );
        let to = Resource::new("test.Widget", "example")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("after".into())),
            )
            .with_attribute("rules", rules(true));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from.clone()),
            to: resolved(to),
            changed_attributes: vec!["name".to_string()],
        };
        let mut plan = Plan::new();
        plan.add(effect);
        let mut historical = HistoricalDisplayAttributes::for_plan(&plan);
        let persisted = PersistedAttributes::from_typed_persisted_wire_values(HashMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            ),
            ("rules".to_string(), rules(true)),
        ]))
        .expect("persisted attributes must encode");
        historical.set_update_from_provider_and_persisted(
            id.clone(),
            &from.attributes,
            Some(&persisted),
        );
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([
                    ("name".to_string(), ExplicitFields::Leaf),
                    (
                        "rules".to_string(),
                        ExplicitFields::ListElements {
                            elements: vec![ExplicitFields::Struct {
                                children: HashMap::from([
                                    ("port".to_string(), ExplicitFields::Leaf),
                                    (
                                        "opts".to_string(),
                                        ExplicitFields::Struct {
                                            children: HashMap::new(),
                                        },
                                    ),
                                ]),
                            }],
                        },
                    ),
                ]),
            },
        )]);
        let (registry, provider) = crate::provider::comparison_fixture_for_test(
            &registry,
            std::iter::once(plan.effects()[0].resource_id()),
        );

        let rows = build_detail_rows_by_instance(
            &plan.effects()[0],
            Some(&registry),
            DetailLevel::Explicit,
            &historical,
            Some(&explicit),
            &provider,
        )
        .expect("detail display must resolve its comparison");

        assert!(matches!(
            rows.as_slice(),
            [DetailRow::Changed { key, .. }] if key == "name"
        ));
    }

    #[test]
    fn update_rows_render_authored_empty_struct_as_provider_payload() {
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id,
            HashMap::from([(
                "config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "label".to_string(),
                    Value::Concrete(ConcreteValue::String("x".into())),
                )]))),
            )]),
        );
        let empty_config = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        let to =
            Resource::new("test.Widget", "example").with_attribute("config", empty_config.clone());
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["config".to_string()],
        };

        let rows = build_detail_rows(
            &effect,
            Some(&empty_struct_comparison_registry()),
            DetailLevel::Full,
            None,
            None,
        );

        assert!(matches!(
            rows.as_slice(),
            [DetailRow::Changed { key, new, .. }]
                if key == "config" && new == &render_value(&empty_config)
        ));
    }

    fn resource_with_merged_default_tags(
        explicit_tags: IndexMap<String, Value>,
        default_tags: IndexMap<String, Value>,
    ) -> Resource {
        let mut resource = Resource::with_provider("test", "test.Widget", "widget", None)
            .with_attribute("tags", Value::Concrete(ConcreteValue::Map(explicit_tags)));
        let config = ProviderConfig {
            name: "test".to_string(),
            attributes: IndexMap::new(),
            default_tags,
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("test.Widget").attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        crate::provider::merge_default_tags(
            &config,
            std::slice::from_mut(&mut resource),
            &registry,
        )
        .expect("default tags must merge");
        resource
    }

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
            vec![render_other("{ k: v }")],
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
        let effect = Effect::Create(resolved(resource));
        let rows = build_detail_rows(&effect, None, DetailLevel::NamesOnly, None, None);
        assert!(rows.is_empty());
    }

    #[test]
    fn test_create_basic_attributes() {
        let resource = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "bucket",
                Value::Concrete(ConcreteValue::String("my-bucket".into())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".into())),
            );
        let effect = Effect::Create(resolved(resource));
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], DetailRow::Attribute { key, .. } if key == "bucket"));
        assert!(matches!(&rows[1], DetailRow::Attribute { key, .. } if key == "region"));
    }

    #[test]
    fn create_tags_annotate_typed_default_tag_provenance() {
        let mut resource = Resource::with_provider("aws", "s3.Bucket", "my-bucket", None)
            .with_attribute(
                "tags",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "Name".to_string(),
                    Value::Concrete(ConcreteValue::String("my-bucket".into())),
                )]))),
            );
        let config = ProviderConfig {
            name: "aws".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::from([(
                "ManagedBy".to_string(),
                Value::Concrete(ConcreteValue::String("carina".into())),
            )]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("s3.Bucket").attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        crate::provider::merge_default_tags(
            &config,
            std::slice::from_mut(&mut resource),
            &registry,
        )
        .expect("default tags must merge");
        let effect = Effect::Create(resolved(resource));

        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);

        let entries = rows
            .iter()
            .find_map(|row| match row {
                DetailRow::MapExpanded { key, entries } if key == "tags" => Some(entries),
                _ => None,
            })
            .expect("tags must render as an expanded map");
        assert!(entries.iter().any(|entry| {
            entry.key.raw() == "ManagedBy" && entry.annotation.as_deref() == Some("# default_tags")
        }));
        assert!(
            entries
                .iter()
                .any(|entry| { entry.key.raw() == "Name" && entry.annotation.is_none() })
        );
    }

    #[test]
    fn test_update_changed_attributes() {
        let from = State::existing(
            ResourceId::with_identity("s3.Bucket", "my-bucket"),
            [(
                "versioning".to_string(),
                Value::Concrete(ConcreteValue::String("Disabled".into())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket").with_attribute(
            "versioning",
            Value::Concrete(ConcreteValue::String("Enabled".into())),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["versioning".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Changed { key, old, new }
            if key == "versioning"
                && old.plain() == "'Disabled'"
                && new.plain() == "'Enabled'"));
    }

    #[test]
    fn update_tags_annotate_only_desired_default_tag_entries() {
        let to = resource_with_merged_default_tags(
            IndexMap::from([(
                "UserWins".to_string(),
                Value::Concrete(ConcreteValue::String("user-new".into())),
            )]),
            IndexMap::from([
                (
                    "Added".to_string(),
                    Value::Concrete(ConcreteValue::String("added".into())),
                ),
                (
                    "Changed".to_string(),
                    Value::Concrete(ConcreteValue::String("default-new".into())),
                ),
                (
                    "UserWins".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-default".into())),
                ),
            ]),
        );
        let from = State::existing(
            to.id.clone(),
            [(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "Changed".to_string(),
                        Value::Concrete(ConcreteValue::String("default-old".into())),
                    ),
                    (
                        "Removed".to_string(),
                        Value::Concrete(ConcreteValue::String("removed".into())),
                    ),
                    (
                        "UserWins".to_string(),
                        Value::Concrete(ConcreteValue::String("user-old".into())),
                    ),
                ]))),
            )]
            .into_iter()
            .collect(),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["tags".to_string()],
        };

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        let entries = rows
            .iter()
            .find_map(|row| match row {
                DetailRow::MapDiff { key, entries } if key == "tags" => Some(entries.as_slice()),
                _ => None,
            })
            .expect("tags must render as a map diff");

        assert_eq!(
            entries,
            [
                MapDiffEntryIR::Added {
                    key: rendered_key("Added"),
                    value: render_string_text("added"),
                    annotation: Some("# default_tags".to_string()),
                },
                MapDiffEntryIR::Changed {
                    key: rendered_key("Changed"),
                    old: render_string_text("default-old"),
                    new: render_string_text("default-new"),
                    annotation: Some("# default_tags".to_string()),
                },
                MapDiffEntryIR::Removed {
                    key: rendered_key("Removed"),
                    value: render_string_text("removed"),
                },
                MapDiffEntryIR::Changed {
                    key: rendered_key("UserWins"),
                    old: render_string_text("user-old"),
                    new: render_string_text("user-new"),
                    annotation: None,
                },
            ]
        );
    }

    #[test]
    fn update_tags_annotates_defaults_when_old_resource_has_no_tags() {
        let to = resource_with_merged_default_tags(
            IndexMap::new(),
            IndexMap::from([
                (
                    "ManagedBy".to_string(),
                    Value::Concrete(ConcreteValue::String("carina".into())),
                ),
                (
                    "Team".to_string(),
                    Value::Concrete(ConcreteValue::String("x".into())),
                ),
            ]),
        );
        let from = State::existing(to.id.clone(), HashMap::new());
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["tags".to_string()],
        };

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        let entries = rows
            .iter()
            .find_map(|row| match row {
                DetailRow::MapDiff { key, entries } if key == "tags" => Some(entries.as_slice()),
                _ => None,
            })
            .expect("tags must render as a map diff");

        assert_eq!(
            entries,
            [
                MapDiffEntryIR::Added {
                    key: rendered_key("ManagedBy"),
                    value: render_string_text("carina"),
                    annotation: Some("# default_tags".to_string()),
                },
                MapDiffEntryIR::Added {
                    key: rendered_key("Team"),
                    value: render_string_text("x"),
                    annotation: Some("# default_tags".to_string()),
                },
            ]
        );
    }

    #[test]
    fn replace_tags_annotate_only_desired_default_tag_entries() {
        let to = resource_with_merged_default_tags(
            IndexMap::from([(
                "UserWins".to_string(),
                Value::Concrete(ConcreteValue::String("user-new".into())),
            )]),
            IndexMap::from([
                (
                    "Added".to_string(),
                    Value::Concrete(ConcreteValue::String("added".into())),
                ),
                (
                    "Changed".to_string(),
                    Value::Concrete(ConcreteValue::String("default-new".into())),
                ),
                (
                    "UserWins".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-default".into())),
                ),
            ]),
        )
        .with_attribute(
            "replacement_key",
            Value::Concrete(ConcreteValue::String("new".into())),
        );
        let previous_attributes = [
            (
                "replacement_key".to_string(),
                Value::Concrete(ConcreteValue::String("old".into())),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "Changed".to_string(),
                        Value::Concrete(ConcreteValue::String("default-old".into())),
                    ),
                    (
                        "Removed".to_string(),
                        Value::Concrete(ConcreteValue::String("removed".into())),
                    ),
                    (
                        "UserWins".to_string(),
                        Value::Concrete(ConcreteValue::String("user-old".into())),
                    ),
                ]))),
            ),
        ]
        .into_iter()
        .collect();
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(to.clone())));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(to.id.clone()),
            identifier: crate::effect::fixture_applied_identifier("old-widget"),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.replace_display_mut().push(ReplaceDisplayMetadata {
            create_idx: 0,
            delete_idx: 1,
            create_before_destroy: false,
            changed_create_only: ChangedCreateOnly::try_from(vec!["replacement_key".to_string()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            previous_attributes,
        });

        // Saved-plan apply decodes this same Create resource and replacement
        // metadata. Round-trip here so the display path proves that desired
        // default-tag provenance survives serialization without being copied
        // into ReplaceDisplayMetadata.
        let plan =
            serde_json::from_str::<SavedPlan>(&serde_json::to_string(&plan).unwrap()).unwrap();
        let Effect::Create(saved_to) = &plan.effects()[0] else {
            panic!("replacement create effect must survive saved-plan round trip");
        };
        let historical = HistoricalDisplayAttributes::for_plan(&plan);
        let rows = build_replace_detail_rows_from_display(
            &plan,
            saved_to,
            plan.replace_display_info().next().unwrap(),
            &historical,
            None,
            DetailLevel::Explicit,
            None,
        );
        let entries = rows
            .iter()
            .find_map(|row| match row {
                DetailRow::MapDiff { key, entries } if key == "tags" => Some(entries.as_slice()),
                _ => None,
            })
            .expect("tags must render as a map diff during replacement");

        assert_eq!(
            entries,
            [
                MapDiffEntryIR::Added {
                    key: rendered_key("Added"),
                    value: render_string_text("added"),
                    annotation: Some("# default_tags".to_string()),
                },
                MapDiffEntryIR::Changed {
                    key: rendered_key("Changed"),
                    old: render_string_text("default-old"),
                    new: render_string_text("default-new"),
                    annotation: Some("# default_tags".to_string()),
                },
                MapDiffEntryIR::Removed {
                    key: rendered_key("Removed"),
                    value: render_string_text("removed"),
                },
                MapDiffEntryIR::Changed {
                    key: rendered_key("UserWins"),
                    old: render_string_text("user-old"),
                    new: render_string_text("user-new"),
                    annotation: None,
                },
            ]
        );
    }

    #[test]
    fn test_update_hidden_unchanged_in_full_mode() {
        let from = State::existing(
            ResourceId::with_identity("s3.Bucket", "my-bucket"),
            [
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("test".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".into())),
                ),
                (
                    "versioning".to_string(),
                    Value::Concrete(ConcreteValue::String("Disabled".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("test".into())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".into())),
            )
            .with_attribute(
                "versioning",
                Value::Concrete(ConcreteValue::String("Enabled".into())),
            );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
            ResourceId::with_identity("s3.Bucket", "my-bucket"),
            [
                (
                    "authored".to_string(),
                    Value::Concrete(ConcreteValue::String("a".into())),
                ),
                (
                    "server_only".to_string(),
                    Value::Concrete(ConcreteValue::String("s".into())),
                ),
                (
                    "trigger_diff".to_string(),
                    Value::Concrete(ConcreteValue::String("old-value".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute(
                "authored",
                Value::Concrete(ConcreteValue::String("a".into())),
            )
            .with_attribute(
                "server_only",
                Value::Concrete(ConcreteValue::String("s".into())),
            )
            .with_attribute(
                "trigger_diff",
                Value::Concrete(ConcreteValue::String("new-value".into())),
            );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
            ResourceId::with_identity("s3.Bucket", "my-bucket"),
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
        let id = ResourceId::with_identity("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::effect::fixture_applied_identifier("old-bucket"),
            generation: crate::effect::EffectGeneration::Current,
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut delete_attrs: HashMap<ResourceId, HashMap<String, Value>> = HashMap::new();
        delete_attrs.insert(
            id.clone(),
            [(
                "bucket".to_string(),
                Value::Concrete(ConcreteValue::String("old-bucket".into())),
            )]
            .into_iter()
            .collect(),
        );
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, Some(&delete_attrs), None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Attribute { key, .. } if key == "bucket"));
    }

    #[test]
    fn delete_identifier_fallback_uses_the_typed_delete_identifier() {
        let id = ResourceId::with_identity("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(id),
            identifier: crate::effect::fixture_applied_identifier("old-bucket"),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };

        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None, None);
        assert!(
            rows.is_empty(),
            "the general builder must not add TUI-only rows"
        );

        let Some(DetailRow::Attribute { key, value, .. }) = delete_identifier_fallback_row(&effect)
        else {
            panic!("expected an identifier fallback row, got {rows:?}");
        };
        assert_eq!(key, "identifier");
        assert_eq!(value.plain(), "old-bucket");

        let create = Effect::Create(resolved(Resource::new("s3.Bucket", "new-bucket")));
        assert!(delete_identifier_fallback_row(&create).is_none());
    }

    #[test]
    fn test_update_removed_attribute() {
        let from = State::existing(
            ResourceId::with_identity("s3.Bucket", "my-bucket"),
            [
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("test".into())),
                ),
                (
                    "removed_attr".to_string(),
                    Value::Concrete(ConcreteValue::String("old-val".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("s3.Bucket", "my-bucket").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["removed_attr".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DetailRow::Removed { key, .. } if key == "removed_attr"));
    }

    #[test]
    fn replace_detail_rows_from_display_diff_previous_attributes_against_create() {
        let to = Resource::new("ec2.Vpc", "vpc").with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        );
        let previous_attributes = HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(to.clone())));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(to.id.clone()),
            identifier: crate::effect::fixture_applied_identifier("vpc-old"),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.replace_display_mut().push(ReplaceDisplayMetadata {
            create_idx: 0,
            delete_idx: 1,
            create_before_destroy: false,
            changed_create_only: ChangedCreateOnly::try_from(vec!["cidr_block".to_string()])
                .unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            previous_attributes,
        });
        let replacement = plan.replace_display_info().next().unwrap();
        let historical = HistoricalDisplayAttributes::for_plan(&plan);

        let rows = build_replace_detail_rows_from_display(
            &plan,
            &to,
            replacement,
            &historical,
            None,
            DetailLevel::Explicit,
            None,
        );

        assert_eq!(
            rows,
            vec![DetailRow::ChangedForcesReplacement {
                key: "cidr_block".to_string(),
                old: render_string_text("10.0.0.0/16"),
                new: render_string_text("10.1.0.0/16"),
            }]
        );
    }

    #[test]
    fn persisted_delete_view_keeps_replacement_desired_secret_mask() {
        const PLAINTEXT: &str = "hunter2";
        let to = Resource::new("test.Widget", "widget").with_attribute(
            "comment",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("rotated".into()),
            )))),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(to.clone())));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(to.id.clone()),
            identifier: crate::effect::fixture_applied_identifier("old-widget"),
            generation: EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique()),
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.replace_display_mut().push(ReplaceDisplayMetadata {
            create_idx: 0,
            delete_idx: 1,
            create_before_destroy: false,
            changed_create_only: ChangedCreateOnly::try_from(vec!["comment".to_string()]).unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            previous_attributes: HashMap::from([(
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String(PLAINTEXT.into())),
            )]),
        });

        let mut historical = HistoricalDisplayAttributes::for_plan(&plan);
        let Effect::Delete {
            id,
            identifier,
            generation,
            ..
        } = &plan.effects()[1]
        else {
            unreachable!()
        };
        let delete_key = DeletedInstanceKey::new(
            id.as_inner().clone(),
            generation.clone(),
            identifier.to_string(),
        );
        let persisted = PersistedAttributes::from_persisted_values(HashMap::from([(
            "comment".to_string(),
            crate::value::PersistedValue::encode_provider_value(
                &Value::Concrete(ConcreteValue::String(PLAINTEXT.into())),
                None,
            )
            .expect("encode persisted comment"),
        )]))
        .expect("persisted attributes");
        historical.set_delete_from_persisted(delete_key, &persisted);

        let rows = build_replace_detail_rows_from_display(
            &plan,
            &to,
            plan.replace_display_info().next().unwrap(),
            &historical,
            None,
            DetailLevel::Explicit,
            None,
        );
        let rendered = format!("{rows:?}");
        assert!(
            !rendered.contains(PLAINTEXT),
            "detail rows leaked: {rendered}"
        );
        assert!(
            rendered.contains("(secret)"),
            "secret was not masked: {rendered}"
        );
    }

    #[test]
    fn update_removed_list_of_maps_attribute_emits_list_diff_row() {
        let mut rule = IndexMap::new();
        rule.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("legacy".into())),
        );
        rule.insert(
            "priority".to_string(),
            Value::Concrete(ConcreteValue::Int(10)),
        );
        let old_rules = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(rule.clone()),
        )]));
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "beta"),
            [("rules".to_string(), old_rules)].into_iter().collect(),
        );
        let to = Resource::new("test.Widget", "beta");
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["rules".to_string()],
        };

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);

        assert_eq!(
            rows,
            vec![DetailRow::ListOfMapsDiff {
                key: "rules".to_string(),
                block: NonEmptyListOfMapsBlock::from_parts(
                    vec![],
                    vec![],
                    vec![],
                    vec![ListOfMapsDiffItem {
                        fields: vec![
                            (
                                rendered_key("name"),
                                pretty_value(Value::Concrete(ConcreteValue::String(
                                    "legacy".into(),
                                ))),
                            ),
                            (
                                rendered_key("priority"),
                                pretty_value(Value::Concrete(ConcreteValue::Int(10)))
                            ),
                        ],
                    }],
                )
                .unwrap(),
            }]
        );
    }

    #[test]
    fn update_legacy_leaf_list_authoring_keeps_element_level_display() {
        let rule = |description: Option<&str>| {
            let mut fields =
                IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let id = ResourceId::with_identity("test.Listener", "listener");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![rule(Some("web")), rule(None)])),
            )]),
        );
        let to = Resource::new("test.Listener", "listener").with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![rule(None), rule(None)])),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["rules".to_string()],
        };
        let explicit = HashMap::from([(
            id,
            crate::explicit::ExplicitFields::Struct {
                children: HashMap::from([(
                    "rules".to_string(),
                    crate::explicit::ExplicitFields::Leaf,
                )]),
            },
        )]);

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, Some(&explicit));

        assert!(
            rows.iter()
                .any(|row| matches!(row, DetailRow::ListOfMapsDiff { key, .. } if key == "rules")),
            "legacy Leaf must retain the ordinary element-level display: {rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, DetailRow::Changed { key, .. } if key == "rules")),
            "legacy Leaf must not opt into whole-value display: {rows:#?}"
        );
    }

    #[test]
    fn update_whole_value_list_avoids_element_identity_display() {
        let rule = |description: Option<&str>| {
            let mut fields =
                IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let id = ResourceId::with_identity("test.Listener", "listener");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![rule(Some("web")), rule(None)])),
            )]),
        );
        let to = Resource::new("test.Listener", "listener").with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![rule(None), rule(None)])),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["rules".to_string()],
        };
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "rules".to_string(),
                    ExplicitFields::WholeValue {
                        pending: PendingPaths::try_new([vec!["description".to_string()]]).unwrap(),
                    },
                )]),
            },
        )]);

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, Some(&explicit));

        assert!(
            rows.iter()
                .any(|row| matches!(row, DetailRow::Changed { key, .. } if key == "rules")),
            "WholeValue must render the list without element pairing: {rows:#?}"
        );
        assert!(format!("{rows:#?}").contains("description"));
    }

    #[test]
    fn update_nested_whole_value_list_avoids_element_identity_display() {
        let rule = |description: Option<&str>| {
            let mut fields =
                IndexMap::from([("port".to_string(), Value::Concrete(ConcreteValue::Int(80)))]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let settings = |rules| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(rules)),
            )])))
        };
        let id = ResourceId::with_identity("test.Listener", "listener");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "settings".to_string(),
                settings(vec![rule(Some("web")), rule(None)]),
            )]),
        );
        let to = Resource::new("test.Listener", "listener")
            .with_attribute("settings", settings(vec![rule(None), rule(None)]));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["settings".to_string()],
        };
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "settings".to_string(),
                    ExplicitFields::Struct {
                        children: HashMap::from([(
                            "rules".to_string(),
                            ExplicitFields::WholeValue {
                                pending: PendingPaths::try_new([vec!["description".to_string()]])
                                    .unwrap(),
                            },
                        )]),
                    },
                )]),
            },
        )]);

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, Some(&explicit));

        let [DetailRow::MapDiff { key, entries }] = rows.as_slice() else {
            panic!("nested WholeValue must stay inside the parent map row: {rows:#?}");
        };
        assert_eq!(key, "settings");
        assert!(matches!(
            entries.as_slice(),
            [MapDiffEntryIR::Changed { key, .. }] if key.raw() == "rules"
        ));
    }

    #[test]
    fn update_whole_value_nested_inside_list_element_avoids_inner_element_pairing() {
        let rule = |port: i64, description: Option<&str>, enabled: bool| {
            let mut fields = IndexMap::from([
                (
                    "port".to_string(),
                    Value::Concrete(ConcreteValue::Int(port)),
                ),
                (
                    "enabled".to_string(),
                    Value::Concrete(ConcreteValue::Bool(enabled)),
                ),
            ]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let listener = |rules| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("primary".into())),
                ),
                (
                    "settings".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "rules".to_string(),
                        Value::Concrete(ConcreteValue::List(rules)),
                    )]))),
                ),
            ])))
        };
        let id = ResourceId::with_identity("test.ListenerSet", "listeners");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "listeners".to_string(),
                Value::Concrete(ConcreteValue::List(vec![listener(vec![
                    rule(80, Some("web"), true),
                    rule(443, None, true),
                ])])),
            )]),
        );
        let to = Resource::new("test.ListenerSet", "listeners").with_attribute(
            "listeners",
            Value::Concrete(ConcreteValue::List(vec![listener(vec![
                rule(80, None, false),
                rule(443, None, true),
            ])])),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("listener-set-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["listeners".to_string()],
        };
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "listeners".to_string(),
                    ExplicitFields::ListElements {
                        elements: vec![ExplicitFields::Struct {
                            children: HashMap::from([
                                ("name".to_string(), ExplicitFields::Leaf),
                                (
                                    "settings".to_string(),
                                    ExplicitFields::Struct {
                                        children: HashMap::from([(
                                            "rules".to_string(),
                                            ExplicitFields::WholeValue {
                                                pending: PendingPaths::try_new([vec![
                                                    "description".to_string(),
                                                ]])
                                                .unwrap(),
                                            },
                                        )]),
                                    },
                                ),
                            ]),
                        }],
                    },
                )]),
            },
        )]);

        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, Some(&explicit));

        let [DetailRow::ListOfMapsDiff { key, block }] = rows.as_slice() else {
            panic!("outer list must retain its element-level row: {rows:#?}");
        };
        assert_eq!(key, "listeners");
        let [modified] = block.modified() else {
            panic!("outer list must have one modified element: {block:#?}");
        };
        let Some(ListOfMapsDiffField::NestedMapChanged { key, entries }) = modified
            .fields
            .as_slice()
            .iter()
            .find(|field| matches!(field, ListOfMapsDiffField::NestedMapChanged { key, .. } if key.raw() == "settings"))
        else {
            panic!("settings must render as a nested map: {modified:#?}");
        };
        assert_eq!(key.raw(), "settings");
        assert!(matches!(
            entries.as_slice(),
            [MapDiffEntryIR::Changed { key, .. }] if key.raw() == "rules"
        ));
    }

    #[test]
    fn deleting_a_whole_list_element_keeps_aligned_element_display() {
        let rule = |port, description: Option<&str>| {
            let mut fields = IndexMap::from([(
                "port".to_string(),
                Value::Concrete(ConcreteValue::Int(port)),
            )]);
            if let Some(description) = description {
                fields.insert(
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String(description.to_string().into())),
                );
            }
            Value::Concrete(ConcreteValue::Map(fields))
        };
        let applied_rules = Value::Concrete(ConcreteValue::List(vec![
            rule(80, Some("web")),
            rule(443, None),
        ]));
        let desired_rules = Value::Concrete(ConcreteValue::List(vec![rule(443, None)]));
        let applied: Resource = Resource::new("test.Listener", "listener")
            .with_attribute("rules", applied_rules.clone());
        let desired: Resource =
            Resource::new("test.Listener", "listener").with_attribute("rules", desired_rules);
        let stored = HashMap::from([("rules".to_string(), applied_rules.clone())]);
        let sent = crate::executor::AcceptedRequest::create_for_test();
        let sent_desired = sent.sent_desired_attributes(&applied);
        let prior = crate::explicit::build_from_resource_for_stored_values_after_send(
            &applied,
            None,
            &stored,
            &sent_desired,
            None,
            &crate::differ::exact_comparison_context_for_tests(),
        );
        let carried = crate::explicit::build_carried_from_resource_for_stored_values(
            &desired,
            &stored,
            &prior,
            &stored,
            None,
            &crate::differ::exact_comparison_context_for_tests(),
        );
        let ExplicitFields::Struct { children } = &carried else {
            panic!("expected carried root Struct");
        };
        assert!(matches!(
            children["rules"],
            ExplicitFields::ListElements { .. }
        ));

        let id = desired.id.clone();
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            from: Box::new(State::existing(id.clone(), stored)),
            to: resolved(desired),
            changed_attributes: vec!["rules".to_string()],
        };
        let explicit = HashMap::from([(id, carried)]);
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, Some(&explicit));

        assert!(
            rows.iter()
                .any(|row| matches!(row, DetailRow::ListOfMapsDiff { key, .. } if key == "rules")),
            "whole-element deletion must retain element-level display: {rows:#?}"
        );
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row, DetailRow::Changed { key, .. } if key == "rules"))
        );
    }

    #[test]
    fn test_create_map_expanded() {
        let mut tags = IndexMap::new();
        tags.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        tags.insert(
            "Environment".to_string(),
            Value::Concrete(ConcreteValue::String("prod".into())),
        );
        let resource = Resource::new("s3.Bucket", "my-bucket")
            .with_attribute("tags", Value::Concrete(ConcreteValue::Map(tags)));
        let effect = Effect::Create(resolved(resource));
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            DetailRow::MapExpanded { key, entries } => {
                assert_eq!(key, "tags");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].key.plain(), "Environment");
                assert_eq!(
                    entries[0].value.value(),
                    &Value::Concrete(ConcreteValue::String("prod".into()))
                );
                assert!(entries[0].annotation.is_none());
                assert_eq!(entries[1].key.plain(), "Name");
                assert_eq!(
                    entries[1].value.value(),
                    &Value::Concrete(ConcreteValue::String("test".into()))
                );
                assert!(entries[1].annotation.is_none());
            }
            other => panic!("expected MapExpanded, got {:?}", other),
        }
    }

    /// Regression: a Map attribute whose entry value is itself a list-of-maps
    /// must retain its structure so the renderer can pretty-print it. If
    /// the build phase pre-stringifies entry values, the inline form
    /// `[{...}, {...}]` is locked in and the renderer cannot expand it
    /// onto multiple lines (issue #2409).
    #[test]
    fn test_create_map_expanded_carries_raw_value_for_nested_list_of_maps() {
        let mut statement1 = IndexMap::new();
        statement1.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("AllowRead".into())),
        );
        statement1.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );
        let mut statement2 = IndexMap::new();
        statement2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("DenyWrite".into())),
        );
        statement2.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".into())),
        );
        let mut policy = IndexMap::new();
        policy.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012-10-17".into())),
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
        let effect = Effect::Create(resolved(resource));
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
            .find(|e| e.key.raw() == "statement")
            .expect("expected `statement` entry");
        match stmt_entry.value.value() {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items.len(), 2, "list-of-maps preserved structurally");
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
        let id = ResourceId::with_identity("s3.Bucket", "old-bucket");
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::effect::fixture_applied_identifier("old-bucket"),
            generation: crate::effect::EffectGeneration::Current,
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut tags = IndexMap::new();
        tags.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
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
                assert_eq!(entries[0].key.plain(), "Name");
                assert_eq!(
                    entries[0].value.value(),
                    &Value::Concrete(ConcreteValue::String("test".into()))
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
        // PrettyAttribute so PrettyValue::render's vertical layout applies.
        let id = ResourceId::with_identity("acm.Certificate", "cert");
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(id.clone()),
            identifier: crate::effect::fixture_applied_identifier("cert"),
            generation: crate::effect::EffectGeneration::Current,
            directives: crate::resource::Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut entry = IndexMap::new();
        entry.insert(
            "domain_name".to_string(),
            Value::Concrete(ConcreteValue::String("registry-dev.carina-rs.dev".into())),
        );
        entry.insert(
            "validation_method".to_string(),
            Value::Concrete(ConcreteValue::String("DNS".into())),
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
                pretty_value.unwrap().value(),
                Value::Concrete(ConcreteValue::List(_))
            ),
            "PrettyAttribute should retain the list structure behind its typed wrapper"
        );
    }

    #[test]
    fn test_replace_basic() {
        let from = State::existing(
            ResourceId::with_identity("ec2.Vpc", "my-vpc"),
            [(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("ec2.Vpc", "my-vpc").with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
        );
        let rows = replace_rows_for_test(
            from,
            to,
            &["cidr_block"],
            DetailLevel::Explicit,
            vec![],
            None,
        );
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            &rows[0],
            DetailRow::ChangedForcesReplacement { key, .. } if key == "cidr_block"
        ));
    }

    #[test]
    fn replace_removed_create_only_attribute_emits_detail_row() {
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "beta"),
            [(
                "legacy_token".to_string(),
                Value::Concrete(ConcreteValue::String("tok".into())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "beta");
        let rows = replace_rows_for_test(
            from,
            to,
            &["legacy_token"],
            DetailLevel::Explicit,
            vec![],
            None,
        );

        assert_eq!(
            rows,
            vec![DetailRow::ReplaceRemoved {
                key: "legacy_token".to_string(),
                old: render_string_text("tok"),
            }]
        );
    }

    #[test]
    fn replace_removed_create_only_map_attribute_emits_map_diff_row() {
        let mut settings = IndexMap::new();
        settings.insert(
            "mode".to_string(),
            Value::Concrete(ConcreteValue::String("legacy".into())),
        );
        settings.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        );
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "beta"),
            [(
                "settings".to_string(),
                Value::Concrete(ConcreteValue::Map(settings)),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "beta");
        let rows =
            replace_rows_for_test(from, to, &["settings"], DetailLevel::Explicit, vec![], None);

        assert_eq!(
            rows,
            vec![DetailRow::MapDiffForcesReplacement {
                key: "settings".to_string(),
                entries: NonEmptyVec::from_vec(vec![
                    MapDiffEntryIR::Removed {
                        key: rendered_key("mode"),
                        value: render_string_text("legacy"),
                    },
                    MapDiffEntryIR::Removed {
                        key: rendered_key("version"),
                        value: render_value(&Value::Concrete(ConcreteValue::Int(1))),
                    },
                ])
                .unwrap(),
            }]
        );
    }

    #[test]
    fn replace_removed_create_only_list_of_maps_attribute_emits_list_diff_row() {
        let mut rule = IndexMap::new();
        rule.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("legacy".into())),
        );
        rule.insert(
            "priority".to_string(),
            Value::Concrete(ConcreteValue::Int(10)),
        );
        let old_rules = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(rule.clone()),
        )]));
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "beta"),
            [("rules".to_string(), old_rules)].into_iter().collect(),
        );
        let to = Resource::new("test.Widget", "beta");
        let rows = replace_rows_for_test(from, to, &["rules"], DetailLevel::Explicit, vec![], None);

        assert_eq!(
            rows,
            vec![DetailRow::ListOfMapsDiffForcesReplacement {
                key: "rules".to_string(),
                block: NonEmptyListOfMapsBlock::from_parts(
                    vec![],
                    vec![],
                    vec![],
                    vec![ListOfMapsDiffItem {
                        fields: vec![
                            (
                                rendered_key("name"),
                                pretty_value(Value::Concrete(ConcreteValue::String(
                                    "legacy".into(),
                                ))),
                            ),
                            (
                                rendered_key("priority"),
                                pretty_value(Value::Concrete(ConcreteValue::Int(10)))
                            ),
                        ],
                    }],
                )
                .unwrap(),
            }]
        );
    }

    #[test]
    fn replace_rows_include_non_forcing_attribute_diffs_and_count_only_equal_attributes() {
        let from = State::existing(
            ResourceId::with_identity("wafv2.WebAcl", "edge"),
            [
                (
                    "name".to_string(),
                    Value::Concrete(ConcreteValue::String("edge-old".into())),
                ),
                (
                    "description".to_string(),
                    Value::Concrete(ConcreteValue::String("old description".into())),
                ),
                (
                    "metric_name".to_string(),
                    Value::Concrete(ConcreteValue::String("edge-old-metric".into())),
                ),
                (
                    "rule_metric_name".to_string(),
                    Value::Concrete(ConcreteValue::String("rule-old".into())),
                ),
                (
                    "scope".to_string(),
                    Value::Concrete(ConcreteValue::String("CLOUDFRONT".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("wafv2.WebAcl", "edge")
            .with_attribute(
                "name",
                Value::Concrete(ConcreteValue::String("edge-new".into())),
            )
            .with_attribute(
                "description",
                Value::Concrete(ConcreteValue::String("new description".into())),
            )
            .with_attribute(
                "metric_name",
                Value::Concrete(ConcreteValue::String("edge-new-metric".into())),
            )
            .with_attribute(
                "rule_metric_name",
                Value::Concrete(ConcreteValue::String("rule-new".into())),
            )
            .with_attribute(
                "scope",
                Value::Concrete(ConcreteValue::String("CLOUDFRONT".into())),
            );
        let rows = replace_rows_for_test(from, to, &["name"], DetailLevel::Full, vec![], None);

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ChangedForcesReplacement { key, old, new }
                    if key == "name"
                        && old.plain() == "'edge-old'"
                        && new.plain() == "'edge-new'"
            )),
            "create-only name diff should still be marked as forcing replacement: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::Changed { key, old, new }
                    if key == "description"
                        && old.plain() == "'old description'"
                        && new.plain() == "'new description'"
            )),
            "non-forcing description diff must render as a normal diff row: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::Changed { key, old, new }
                    if key == "metric_name"
                        && old.plain() == "'edge-old-metric'"
                        && new.plain() == "'edge-new-metric'"
            )),
            "non-forcing metric_name diff must render as a normal diff row: {rows:?}"
        );
        assert_eq!(
            rows.iter().find_map(|row| match row {
                DetailRow::HiddenUnchanged { count } => Some(*count),
                _ => None,
            }),
            Some(1),
            "HiddenUnchanged must count only genuinely equal attributes: {rows:?}"
        );
    }

    #[test]
    fn replace_same_forcing_key_prefers_cascade_ref_hint() {
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [(
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:old".into())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "arn",
            Value::Concrete(ConcreteValue::String("arn:old".into())),
        );
        let rows = replace_rows_for_test(
            from,
            to,
            &["arn"],
            DetailLevel::Explicit,
            vec![("arn".to_string(), "${web_acl.arn}".to_string())],
            None,
        );

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ReplaceCascade { key, old, new }
                    if key == "arn"
                        && old.plain() == "'arn:old'"
                        && new.plain() == "${web_acl.arn}"
            )),
            "same forcing key should render ReplaceCascade with the cascade hint: {rows:?}"
        );
    }

    #[test]
    fn replace_same_forcing_key_without_cascade_ref_hint_formats_new_value() {
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [(
                "arn".to_string(),
                Value::Concrete(ConcreteValue::String("arn:old".into())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "arn",
            Value::Concrete(ConcreteValue::String("arn:old".into())),
        );
        let rows = replace_rows_for_test(from, to, &["arn"], DetailLevel::Explicit, vec![], None);

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ReplaceCascade { key, old, new }
                    if key == "arn"
                        && old.plain() == "'arn:old'"
                        && new.plain() == "'arn:old'"
            )),
            "same forcing key should fall back to formatting the new value: {rows:?}"
        );
    }

    #[test]
    fn replace_forcing_list_of_maps_keeps_row_when_nested_diff_collapses() {
        let mut old_condition = IndexMap::new();
        old_condition.insert(
            "string_equals".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec!["x".into()])),
        );
        let mut new_condition = IndexMap::new();
        new_condition.insert(
            "string_equals".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("x".into()),
            )])),
        );
        let mut old_item = IndexMap::new();
        old_item.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        old_item.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(old_condition)),
        );
        let mut new_item = IndexMap::new();
        new_item.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        new_item.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(new_condition)),
        );
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [(
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(old_item),
                )])),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(new_item),
            )])),
        );
        let rows = replace_rows_for_test(from, to, &["rules"], DetailLevel::Full, vec![], None);

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ForceReplaceListOfMapsHeader { key } if key == "rules"
            )),
            "forcing list-of-maps key must render as a header-only row when nested diffs collapse: {rows:?}"
        );
    }

    #[test]
    fn replace_forcing_map_keeps_row_when_nested_diff_collapses() {
        let mut old_settings = IndexMap::new();
        old_settings.insert(
            "principal".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec![
                "arn:aws:iam::123:oidc-provider/x".into(),
            ])),
        );
        let mut new_settings = IndexMap::new();
        new_settings.insert(
            "principal".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("arn:aws:iam::123:oidc-provider/x".into()),
            )])),
        );
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [(
                "settings".to_string(),
                Value::Concrete(ConcreteValue::Map(old_settings)),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "settings",
            Value::Concrete(ConcreteValue::Map(new_settings)),
        );
        let rows = replace_rows_for_test(from, to, &["settings"], DetailLevel::Full, vec![], None);

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ForceReplaceMapHeader { key } if key == "settings"
            )),
            "forcing map key must render as a header-only row when nested diffs collapse: {rows:?}"
        );
    }

    #[test]
    fn replace_forcing_display_equal_scalar_still_renders_forcing_row() {
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [(
                "aliases".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec!["only-one".into()])),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "aliases",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only-one".into()),
            )])),
        );
        let rows = replace_rows_for_test(from, to, &["aliases"], DetailLevel::Full, vec![], None);

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ChangedForcesReplacement { key, old, new }
                    if key == "aliases"
                        && old.plain() == "['only-one']"
                        && new.plain() == "['only-one']"
            )),
            "forcing display-equal scalar key must remain visible: {rows:?}"
        );
    }

    #[test]
    fn replace_forcing_key_uses_raw_old_value_when_explicit_projection_omits_it() {
        use crate::explicit::ExplicitFields;

        let id = ResourceId::with_identity("test.Widget", "w");
        let from = State::existing(
            id.clone(),
            [
                (
                    "server_defaulted_name".to_string(),
                    Value::Concrete(ConcreteValue::String("old-name".into())),
                ),
                (
                    "authored".to_string(),
                    Value::Concrete(ConcreteValue::String("kept".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w")
            .with_attribute(
                "server_defaulted_name",
                Value::Concrete(ConcreteValue::String("new-name".into())),
            )
            .with_attribute(
                "authored",
                Value::Concrete(ConcreteValue::String("kept".into())),
            );
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([("authored".to_string(), ExplicitFields::Leaf)]),
            },
        )]);

        let rows = replace_rows_for_test(
            from,
            to,
            &["server_defaulted_name"],
            DetailLevel::Full,
            vec![],
            Some(&explicit),
        );

        assert!(
            rows.iter().any(|row| matches!(
                row,
                DetailRow::ChangedForcesReplacement { key, old, new }
                    if key == "server_defaulted_name"
                        && old.plain() == "'old-name'"
                        && new.plain() == "'new-name'"
            )),
            "forcing key should use raw state old value even when projection omits it: {rows:?}"
        );
    }

    #[test]
    fn replace_forcing_list_prefers_projected_old_value_over_raw_server_elements() {
        let keyed = |key: &str, value: &str| {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String(key.to_string().into())),
                ),
                (
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String(value.to_string().into())),
                ),
            ])))
        };
        let id = ResourceId::with_identity("test.Widget", "w");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "attributes".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    keyed("authored", "old"),
                    keyed("server-default", "filled"),
                ])),
            )]),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "attributes",
            Value::Concrete(ConcreteValue::List(vec![keyed("authored", "new")])),
        );
        let authored_element = ExplicitFields::Struct {
            children: HashMap::from([
                ("key".to_string(), ExplicitFields::Leaf),
                ("value".to_string(), ExplicitFields::Leaf),
            ]),
        };
        let explicit = HashMap::from([(
            id,
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "attributes".to_string(),
                    ExplicitFields::ListElements {
                        elements: vec![authored_element, ExplicitFields::Unrecorded],
                    },
                )]),
            },
        )]);

        let rows = replace_rows_for_test(
            from,
            to,
            &["attributes"],
            DetailLevel::Full,
            vec![],
            Some(&explicit),
        );

        let [DetailRow::ListOfMapsDiffForcesReplacement { key, block }] = rows.as_slice() else {
            panic!("forcing list must render a list diff: {rows:#?}");
        };
        assert_eq!(key, "attributes");
        assert_eq!(block.modified().len(), 1);
        assert!(block.removed().is_empty());
        assert!(block.added().is_empty());
    }

    #[test]
    fn replace_rows_include_non_forcing_removed_attributes() {
        let from = State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            [
                (
                    "external_name".to_string(),
                    Value::Concrete(ConcreteValue::String("old-name".into())),
                ),
                (
                    "tag".to_string(),
                    Value::Concrete(ConcreteValue::String("legacy".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("test.Widget", "w").with_attribute(
            "external_name",
            Value::Concrete(ConcreteValue::String("new-name".into())),
        );
        let rows = replace_rows_for_test(
            from,
            to,
            &["external_name"],
            DetailLevel::Explicit,
            vec![],
            None,
        );

        assert!(
            rows.iter()
                .any(|row| matches!(row, DetailRow::Removed { key, old }
                    if key == "tag" && old.plain() == "'legacy'")),
            "replace rows must render non-forcing removed attributes alongside forcing changes: {rows:?}"
        );
    }

    #[test]
    fn create_row_list_of_maps_emits_pretty_attribute() {
        let mut entry = indexmap::IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("S1".into())),
        );
        entry.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );
        let resource = Resource::new("iam.RolePolicy", "test").with_attribute(
            "statement",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(entry),
            )])),
        );
        let effect = Effect::Create(resolved(resource));
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
                pretty_value.unwrap().value(),
                Value::Concrete(ConcreteValue::List(_))
            ),
            "PrettyAttribute should retain the list structure behind its typed wrapper"
        );
    }

    #[test]
    fn create_row_scalar_attribute_unchanged() {
        let resource = Resource::new("iam.Role", "test").with_attribute(
            "role_name",
            Value::Concrete(ConcreteValue::String("foo".into())),
        );
        let effect = Effect::Create(resolved(resource));
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
        // PrettyValue::render's 80-col threshold can apply.
        let resource = Resource::new("iam.Role", "test").with_attribute(
            "managed_policy_arns",
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:iam::aws:policy/Policy1".into(),
                )),
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:iam::aws:policy/Policy2".into(),
                )),
            ])),
        );
        let effect = Effect::Create(resolved(resource));
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
            matches!(
                pretty.unwrap().value(),
                Value::Concrete(ConcreteValue::List(_))
            ),
            "PrettyAttribute should retain the list structure behind its typed wrapper"
        );
    }

    #[test]
    fn create_row_empty_list_emits_pretty_attribute() {
        // Pins down `tags = []` behavior — a regression that re-introduces
        // an `!items.is_empty()` guard would silently bypass the routing
        // for empty lists, breaking the formatting-path uniformity.
        let resource = Resource::new("iam.Role", "test")
            .with_attribute("tags", Value::Concrete(ConcreteValue::List(vec![])));
        let effect = Effect::Create(resolved(resource));
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

    use crate::schema::{
        CompiledAttributeType as AttributeType, CompiledStructField as StructField,
    };

    /// Build a registry whose `iam.Role.policy` attribute is the
    /// IAM-policy-doc shape: a Struct with a Enum `version` and a
    /// `List<Struct{ effect: Enum }>` `statement`. `dsl_aliases`
    /// map the API spelling to the DSL alias, mirroring the real
    /// provider schema (`Allow`↔`allow`, `2012-10-17`↔`2012_10_17`).
    fn iam_policy_registry() -> SchemaRegistry {
        let effect = PatternAttributeType::enum_(
            crate::schema::TypeIdentity::bare("Effect"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );
        let version = PatternAttributeType::enum_(
            crate::schema::TypeIdentity::bare("Version"),
            Some(vec!["2012-10-17".to_string(), "2008-10-17".to_string()]),
            vec![
                ("2012-10-17".to_string(), "2012_10_17".to_string()),
                ("2008-10-17".to_string(), "2008_10_17".to_string()),
            ],
            None,
            None,
        );
        let statement = PatternAttributeType::unordered_list(PatternAttributeType::struct_(
            "Statement".to_string(),
            vec![
                PatternStructField::optional("effect", effect).build(),
                PatternStructField::optional("action", PatternAttributeType::string()).build(),
            ],
        ));
        let policy = PatternAttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![
                PatternStructField::optional("version", version).build(),
                PatternStructField::optional("statement", statement).build(),
            ],
        );
        let schema = PatternResourceSchema::new("iam.Role")
            .attribute(PatternAttributeSchema::optional("policy", policy).build())
            .attribute(
                PatternAttributeSchema::optional("description", PatternAttributeType::string())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional("region", PatternAttributeType::string()).build(),
            );
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema.validate().expect("schema fixture must be valid"));
        registry
    }

    fn iam_policy_value(effect: ConcreteValue, version: ConcreteValue) -> Value {
        let mut stmt = indexmap::IndexMap::new();
        stmt.insert("effect".to_string(), Value::Concrete(effect));
        stmt.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
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
            ResourceId::with_identity("iam.Role", "r"),
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
                ConcreteValue::String("Allow".into()),
                ConcreteValue::String("2012-10-17".into()),
            ),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["policy".to_string()],
        };
        let registry = iam_policy_registry();
        let rows = build_detail_rows(&effect, Some(&registry), DetailLevel::Explicit, None, None);
        assert!(
            rows.is_empty(),
            "enum-equal-only policy must produce no phantom row, got: {rows:?}"
        );
    }

    #[test]
    fn update_with_reference_form_equivalent_leaf_emits_no_phantom_row() {
        use crate::provider::ReferenceScope;
        use crate::schema::{ReferenceForm, ReferenceForms, ReferenceTemplate, TypeIdentity};

        let identity = TypeIdentity::new(Some("aws"), ["lambda", "Function"], "Name");
        let attr_type =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let schema = PatternResourceSchema::new("lambda.Permission")
            .attribute(PatternAttributeSchema::optional("function_name", attr_type).build());
        let forms = ReferenceForms::from([(
            identity,
            ReferenceForm::Template {
                long: ReferenceTemplate::new(
                    "arn:{partition}:lambda:{region}:{account}:function:{name}",
                )
                .expect("valid reference template"),
            },
        )]);
        let id = ResourceId::with_provider_identity("aws", "lambda.Permission", "invoke", None);
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "function_name".to_string(),
                Value::Concrete(ConcreteValue::String(
                    "arn:aws:lambda:us-east-1:123456789012:function:processor".into(),
                )),
            )]),
        );
        let to = Resource::with_provider("aws", "lambda.Permission", "invoke", None)
            .with_attribute(
                "function_name",
                Value::Concrete(ConcreteValue::String("processor".into())),
            );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("permission-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["function_name".to_string()],
        });
        let historical = HistoricalDisplayAttributes::for_plan(&plan);
        let scope = ReferenceScope::available("aws", "us-east-1", "123456789012")
            .expect("fixture scope is valid");
        let witness = crate::schema::ValidatedProviderSchemaWitness::new(
            vec![schema.validate().expect("valid schema")],
            crate::schema::ValidatedConfigSchema::default(),
            forms,
        )
        .expect("valid provider schema witness");
        let mut registry = SchemaRegistry::new();
        registry
            .insert_provider_witness("aws", &witness)
            .expect("reference forms must be admitted with their schema");
        let (registry, provider) = crate::provider::comparison_fixture_with_scope_for_test(
            &registry,
            std::iter::once(&id),
            scope,
        );

        let rows = build_detail_rows_by_instance(
            &plan.effects()[0],
            Some(&registry),
            DetailLevel::Explicit,
            &historical,
            None,
            &provider,
        )
        .expect("reference-form display must resolve through the provider router");

        assert!(
            rows.is_empty(),
            "equivalent reference emitted rows: {rows:?}"
        );
    }

    #[test]
    fn update_rows_require_a_schema_backed_provider_comparison() {
        let id = ResourceId::with_identity("test.Widget", "example");
        let from = State::existing(
            id,
            HashMap::from([(
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("old".into())),
            )]),
        )
        .with_identifier("widget-id");
        let to = Resource::new("test.Widget", "example")
            .with_attribute("name", Value::Concrete(ConcreteValue::String("new".into())));
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("widget-id"),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["name".to_string()],
        });
        let historical = HistoricalDisplayAttributes::for_plan(&plan);

        let error = build_detail_rows_by_instance(
            &plan.effects()[0],
            None,
            DetailLevel::Explicit,
            &historical,
            None,
            &ProviderRouter::new(),
        )
        .expect_err("an update without a schema/comparison must fail");

        assert!(
            error.to_string().contains("schema"),
            "typed error must identify the missing schema: {error}"
        );
    }

    /// A genuinely-changed sibling (`description`) must still render,
    /// and the enum-equal `policy` must NOT add a phantom row alongside
    /// it. Guards against the fix over-suppressing.
    #[test]
    fn update_real_sibling_change_still_renders_without_enum_phantom() {
        let from = State::existing(
            ResourceId::with_identity("iam.Role", "r"),
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
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r")
            .with_attribute(
                "policy",
                iam_policy_value(
                    ConcreteValue::String("Allow".into()),
                    ConcreteValue::String("2012-10-17".into()),
                ),
            )
            .with_attribute(
                "description",
                Value::Concrete(ConcreteValue::String("new".into())),
            );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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

    /// With an empty resource schema, enum-shaped values have no declared enum
    /// aliases, so the shared exact comparison keeps the raw-spelling change.
    #[test]
    fn update_with_empty_schema_uses_exact_comparison() {
        let from = State::existing(
            ResourceId::with_identity("iam.Role", "r"),
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
                ConcreteValue::String("Allow".into()),
                ConcreteValue::String("2012-10-17".into()),
            ),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["policy".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert!(
            !rows.is_empty(),
            "the empty-schema comparison must still show the raw-spelling diff"
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
            ResourceId::with_identity("iam.Role", "r"),
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
                ConcreteValue::String("Allow".into()),
                ConcreteValue::String("2012-10-17".into()),
            ),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
        let enum_t = PatternAttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec!["On".to_string(), "Off".to_string()]),
            vec![
                ("On".to_string(), "on".to_string()),
                ("Off".to_string(), "off".to_string()),
            ],
            None,
            None,
        );
        let map_t = PatternAttributeType::map_with_key(PatternAttributeType::string(), enum_t);
        let schema = PatternResourceSchema::new("x.Thing")
            .attribute(PatternAttributeSchema::optional("modes", map_t).build());
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema.validate().expect("schema fixture must be valid"));

        let mk = |v: ConcreteValue| {
            let mut m = indexmap::IndexMap::new();
            m.insert("a".to_string(), Value::Concrete(v));
            Value::Concrete(ConcreteValue::Map(m))
        };
        let from = State::existing(
            ResourceId::with_identity("x.Thing", "t"),
            [(
                "modes".to_string(),
                mk(ConcreteValue::enum_identifier("on".to_string())),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t")
            .with_attribute("modes", mk(ConcreteValue::String("On".into())));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
            ResourceId::with_identity("iam.Role", "r"),
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
                ConcreteValue::String("Deny".into()),
                ConcreteValue::String("2012-10-17".into()),
            ),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
    /// attributes. Guards the full tally invariant, not
    /// just `count == 1` in isolation.
    #[test]
    fn update_full_mode_multi_attr_tally_balances() {
        let from = State::existing(
            ResourceId::with_identity("iam.Role", "r"),
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
                    Value::Concrete(ConcreteValue::String("old".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us-east-1".into())),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("iam.Role", "r")
            .with_attribute(
                "policy",
                iam_policy_value(
                    ConcreteValue::String("Allow".into()),
                    ConcreteValue::String("2012-10-17".into()),
                ),
            )
            .with_attribute(
                "description",
                Value::Concrete(ConcreteValue::String("new".into())),
            )
            .with_attribute(
                "region",
                Value::Concrete(ConcreteValue::String("us-east-1".into())),
            );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
        // total attributes.
        assert_eq!(changed_rows + hidden.unwrap(), 3);
    }

    fn modes_registry() -> SchemaRegistry {
        let mode = PatternAttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );
        let schema = PatternResourceSchema::new("x.Thing").attribute(
            PatternAttributeSchema::optional("modes", PatternAttributeType::unordered_list(mode))
                .build(),
        );
        let mut registry = SchemaRegistry::new();
        registry.insert("", schema.validate().expect("schema fixture must be valid"));
        registry
    }

    #[test]
    fn enum_list_diff_keeps_dotted_string_elements_literal() {
        let attr_type = AttributeType::unordered_list(AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Format"),
            Some(vec!["json".to_string()]),
            Vec::new(),
            None,
            None,
        ));
        let old = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("A.json".into()),
        )]));
        let new = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("json".into()),
        )]));

        let diff = compute_string_list_change(
            Some(&old),
            &new,
            Some(&attr_type),
            crate::schema::empty_defs_for_schema_walks(),
        )
        .expect("literal dotted strings must produce a visible list diff");

        assert_eq!(
            diff.removed
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["'A.json'"]
        );
        assert_eq!(
            diff.added
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["'json'"]
        );
    }

    #[test]
    fn enum_list_diff_case_folds_literal_strings_like_the_differ() {
        let attr_type = AttributeType::unordered_list(AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec!["Allow".to_string(), "X".to_string(), "Y".to_string()]),
            Vec::new(),
            None,
            None,
        ));
        let old = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("allow".into())),
            Value::Concrete(ConcreteValue::String("X".into())),
        ]));
        let new = enum_list(&["Allow", "Y"]);

        let diff = compute_string_list_change(
            Some(&old),
            &new,
            Some(&attr_type),
            crate::schema::empty_defs_for_schema_walks(),
        )
        .expect("X → Y must remain a visible list diff");

        assert_eq!(
            diff.unchanged
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["Allow"]
        );
        assert_eq!(
            diff.removed
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["'X'"]
        );
        assert_eq!(
            diff.added
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["Y"]
        );
    }

    #[test]
    fn enum_list_diff_keeps_each_duplicate_and_cross_shape_display_attached() {
        let attr_type = AttributeType::unordered_list(AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Mode"),
            Some(vec![
                "Allow".to_string(),
                "Deny".to_string(),
                "Legacy".to_string(),
            ]),
            Vec::new(),
            None,
            None,
        ));
        let old = Value::Concrete(ConcreteValue::StringList(vec![
            "allow".into(),
            "legacy".into(),
        ]));
        let new = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::enum_identifier("Allow")),
            Value::Concrete(ConcreteValue::String("allow".into())),
            Value::Concrete(ConcreteValue::enum_identifier("Deny")),
        ]));

        let diff = compute_string_list_change(
            Some(&old),
            &new,
            Some(&attr_type),
            crate::schema::empty_defs_for_schema_walks(),
        )
        .expect("Deny and legacy must produce a visible list diff");

        assert_eq!(
            diff.unchanged
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["Allow", "'allow'"]
        );
        assert_eq!(
            diff.added
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["Deny"]
        );
        assert_eq!(
            diff.removed
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["'legacy'"]
        );
    }

    #[test]
    fn enum_list_diff_preserves_dotted_api_values() {
        for api_value in ["TLSv1.1", "ipsec.1"] {
            let attr_type = AttributeType::unordered_list(AttributeType::enum_(
                crate::schema::TypeIdentity::bare("Protocol"),
                Some(vec![api_value.to_string()]),
                Vec::new(),
                None,
                None,
            ));
            let old = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String(api_value.into()),
            )]));
            let new = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::CanonicalEnum(crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::TypeIdentity::bare("Protocol"),
                    api_value,
                )),
            )]));

            assert!(
                compute_string_list_change(
                    Some(&old),
                    &new,
                    Some(&attr_type),
                    crate::schema::empty_defs_for_schema_walks(),
                )
                .is_none(),
                "{api_value} must remain one literal API value"
            );
        }
    }

    #[test]
    fn enum_list_diff_uses_the_same_dsl_fold_as_the_differ() {
        let attr_type = AttributeType::unordered_list(AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Hostname"),
            Some(vec!["FriendlyNameType".to_string()]),
            Vec::new(),
            None,
            Some(crate::schema::DslTransform::StripSuffix("Type".to_string())),
        ));
        let old = enum_list(&["FriendlyNameType"]);
        let new = enum_list(&["FriendlyName"]);

        assert!(
            compute_string_list_change(
                Some(&old),
                &new,
                Some(&attr_type),
                crate::schema::empty_defs_for_schema_walks(),
            )
            .is_none(),
            "the renderer must use the differ's API → DSL → API fold"
        );
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

    #[test]
    fn map_string_list_change_resolves_ref_union_member_enum() {
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
        let settings = AttributeType::struct_(
            "Settings",
            vec![
                crate::schema::CompiledStructField::optional(
                    "modes",
                    AttributeType::union(vec![AttributeType::ref_("L")]),
                )
                .build(),
            ],
        );
        let defs = std::collections::BTreeMap::from([("L".to_string(), AttributeType::list(mode))]);
        let old_value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "modes".to_string(),
            enum_list(&["allow"]),
        )])));
        let new_value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "modes".to_string(),
            enum_list(&["Allow", "Deny"]),
        )])));

        let entries = compute_map_diff_entries(
            Some(&old_value),
            &new_value,
            Some(&settings),
            &defs,
            DetailLevel::Explicit,
        );

        assert_eq!(entries.len(), 1, "expected one nested row: {entries:?}");
        let MapDiffEntryIR::StringListChanged {
            key,
            unchanged,
            added,
            removed,
        } = &entries[0]
        else {
            panic!("expected StringListChanged, got: {entries:?}");
        };
        assert_eq!(key.plain(), "modes");
        assert_eq!(
            unchanged
                .iter()
                .map(RenderedValue::plain)
                .collect::<Vec<_>>(),
            vec!["Allow"]
        );
        assert_eq!(
            added.iter().map(RenderedValue::plain).collect::<Vec<_>>(),
            vec!["Deny"]
        );
        assert!(removed.is_empty(), "no alias-only removal: {removed:?}");
    }

    #[test]
    fn string_list_inner_type_finds_list_through_cyclic_ref_union() {
        let attr_type = AttributeType::union(vec![AttributeType::ref_("L")]);
        let defs = std::collections::BTreeMap::from([(
            "L".to_string(),
            AttributeType::union(vec![
                AttributeType::ref_("L"),
                AttributeType::list(AttributeType::string()),
            ]),
        )]);

        let inner = string_list_inner_type(&attr_type, &defs)
            .expect("the concrete List sibling must remain reachable");

        assert!(matches!(
            inner.shape_ref_free(),
            Ok(crate::schema::Shape::String { .. })
        ));
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
            ResourceId::with_identity("x.Thing", "t"),
            [("modes".to_string(), enum_list(&["allow"]))]
                .into_iter()
                .collect(),
        );
        let to =
            Resource::new("x.Thing", "t").with_attribute("modes", enum_list(&["Allow", "Deny"]));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
            added[0].plain().eq_ignore_ascii_case("deny"),
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
                ConcreteValue::String("sts:AssumeRoleWithWebIdentity".into()),
            )])),
        );
        // The shape-mismatched field that breaks Phase 1 exact match.
        old_oidc.insert(
            "principal_federated".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec![
                "arn:aws:iam::123:oidc-provider/token.example.com"
                    .to_string()
                    .into(),
            ])),
        );

        let mut new_oidc = indexmap::IndexMap::new();
        new_oidc.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("sts:AssumeRoleWithWebIdentity".into()),
            )])),
        );
        new_oidc.insert(
            "principal_federated".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String(
                    "arn:aws:iam::123:oidc-provider/token.example.com"
                        .to_string()
                        .into(),
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
            Value::Concrete(ConcreteValue::String("sts:AssumeRole".into())),
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
                "arn:aws:iam::123:oidc-provider/x".into(),
            ])),
        );
        // A genuinely different sibling key forces the parent map onto
        // the Changed path so the recursion is exercised.
        old_principal.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("old.example.com".into())),
        );

        let mut new_principal = indexmap::IndexMap::new();
        new_principal.insert(
            "federated".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("arn:aws:iam::123:oidc-provider/x".into()),
            )])),
        );
        new_principal.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("new.example.com".into())),
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
            if let MapDiffEntryIR::Changed { key, old, new, .. } = entry {
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
            ResourceId::with_identity("x.Thing", "t"),
            [(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec!["only-one".into()])),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t").with_attribute(
            "tags",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only-one".into()),
            )])),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["tags".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Full, None, None);
        for row in &rows {
            if let DetailRow::Changed { key, old, new, .. } = row {
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
            ResourceId::with_identity("x.Thing", "t"),
            [(
                "password".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("old-secret".into()),
                )))),
            )]
            .into_iter()
            .collect(),
        );
        let to = Resource::new("x.Thing", "t").with_attribute(
            "password",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("new-secret".into()),
            )))),
        );
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
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
            Value::Concrete(ConcreteValue::StringList(vec!["x".into()])),
        );
        let mut new_condition = indexmap::IndexMap::new();
        new_condition.insert(
            "string_equals".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("x".into()),
            )])),
        );

        // The whole statement mirrors the real OIDC repro: many
        // shared fields (`sid`, `effect`, `action`) drive Phase 2
        // similarity high enough to pair; the `condition` nested map
        // is the all-phantom field that triggers the bug.
        let mut old_stmt = indexmap::IndexMap::new();
        old_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        old_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );
        old_stmt.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(old_condition)),
        );

        let mut new_stmt = indexmap::IndexMap::new();
        new_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        new_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
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
                ConcreteValue::String("old".into()),
            )))),
        );
        let mut new_map = indexmap::IndexMap::new();
        new_map.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("new".into()),
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
                MapDiffEntryIR::Changed { key, .. } if key.raw() == "password"
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
            ConcreteValue::String("old".into()),
        ))));
        let secret_new = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("new".into()),
        ))));

        // Same `sid` on both sides so the similarity score is positive → Phase 2
        // pairs them. A different `effect` key forces Phase 1
        // exact-match to fail so the pair is *modified*, not unchanged.
        let mut old_stmt = indexmap::IndexMap::new();
        old_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        old_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );
        old_stmt.insert("password".to_string(), secret_old);

        let mut new_stmt = indexmap::IndexMap::new();
        new_stmt.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("shared".into())),
        );
        new_stmt.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".into())),
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
                .any(|f| matches!(f, ListOfMapsDiffField::Changed { key, .. } if key.raw() == "password"))
        });
        assert!(
            password_changed,
            "secret rotation as a paired-element peer must keep its Changed field, got: {modified:?}"
        );
    }

    /// carina#3722: a newly inserted statement that appears first must not
    /// steal the only old statement from a more-similar modified statement.
    #[test]
    fn list_of_maps_best_first_pairing_prevents_greedy_steal() {
        let string = |value: &str| Value::Concrete(ConcreteValue::String(value.to_string().into()));
        let strings = |values: &[&str]| {
            Value::Concrete(ConcreteValue::List(
                values.iter().map(|value| string(value)).collect(),
            ))
        };
        let statement = |sid: &str, action: &[&str], resource: &[&str]| {
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                ("sid".to_string(), string(sid)),
                ("effect".to_string(), string("Allow")),
                ("action".to_string(), strings(action)),
                ("resource".to_string(), strings(resource)),
            ])))
        };

        let old_value = Value::Concrete(ConcreteValue::List(vec![statement(
            "ManagePublishStackRoles",
            &["iam:CreateRole", "iam:DeleteRole"],
            &[
                "arn:aws:iam::*:role/carina-registry-publish-api-task",
                "arn:aws:iam::*:role/carina-registry-policy-write",
            ],
        )]));
        let new_value = Value::Concrete(ConcreteValue::List(vec![
            statement(
                "ECRRepository",
                &["ecr:CreateRepository"],
                &["arn:aws:ecr:*:*:repository/carina-registry-publish-api"],
            ),
            statement(
                "ManagePublishStackRoles",
                &["iam:CreateRole", "iam:DeleteRole"],
                &[
                    "arn:aws:iam::*:role/carina-registry-publish-api-task",
                    "arn:aws:iam::*:role/carina-registry-policy-write",
                    "arn:aws:iam::*:role/carina-registry-publish-api-execution",
                ],
            ),
        ]));

        let (_unchanged, modified, added, removed) = compute_list_of_maps_diff_parts(
            Some(&old_value),
            &new_value,
            None,
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        assert_eq!(
            added.len(),
            1,
            "expected only the inserted statement: {added:?}"
        );
        assert!(
            removed.is_empty(),
            "old statement must be paired: {removed:?}"
        );
        assert_eq!(
            modified.len(),
            1,
            "expected the role statement modification"
        );
        assert!(
            !modified[0].fields.as_slice().iter().any(
                |field| matches!(field, ListOfMapsDiffField::Changed { key, .. } if key.raw() == "sid")
            ),
            "the modified statement must retain its sid: {:?}",
            modified[0]
        );
    }

    /// carina#3736 finding 2: similarity uses the element schema too,
    /// not only the exact-match phase surrounding it.
    #[test]
    fn list_of_maps_similarity_pairs_enum_alias_fields_with_schema() {
        let enum_type = |name: &str, api: &str, dsl: &str| {
            AttributeType::enum_(
                crate::schema::TypeIdentity::bare(name),
                Some(vec![api.to_string()]),
                vec![(api.to_string(), dsl.to_string())],
                None,
                None,
            )
        };
        let element_type = AttributeType::struct_(
            "Statement",
            vec![
                StructField::optional("effect", enum_type("Effect", "Allow", "allow")).build(),
                StructField::optional("mode", enum_type("Mode", "Enabled", "enabled")).build(),
                StructField::optional("scope", enum_type("Scope", "Regional", "regional")).build(),
                StructField::optional("description", AttributeType::string()).build(),
            ],
        );
        let list_type = AttributeType::unordered_list(element_type);
        let old_item = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier("allow")),
            ),
            (
                "mode".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier("enabled")),
            ),
            (
                "scope".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier("regional")),
            ),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            ),
        ])));
        let new_item = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::String("Allow".into())),
            ),
            (
                "mode".to_string(),
                Value::Concrete(ConcreteValue::String("Enabled".into())),
            ),
            (
                "scope".to_string(),
                Value::Concrete(ConcreteValue::String("Regional".into())),
            ),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            ),
        ])));
        let old_value = Value::Concrete(ConcreteValue::List(vec![old_item]));
        let new_value = Value::Concrete(ConcreteValue::List(vec![new_item]));

        let (_unchanged, modified, added, removed) = compute_list_of_maps_diff_parts(
            Some(&old_value),
            &new_value,
            Some(&list_type),
            crate::schema::empty_defs_for_schema_walks(),
            DetailLevel::Full,
        );

        assert!(
            added.is_empty(),
            "schema-equivalent enum fields must pair: {added:?}"
        );
        assert!(
            removed.is_empty(),
            "schema-equivalent enum fields must pair: {removed:?}"
        );
        assert_eq!(
            modified.len(),
            1,
            "the genuine sibling change must remain visible"
        );
        assert_eq!(modified[0].fields.as_slice().len(), 1);
        assert!(matches!(
            &modified[0].fields.as_slice()[0],
            ListOfMapsDiffField::Changed { key, .. } if key.raw() == "description"
        ));
    }

    /// With an empty resource schema, the shared exact comparison has no enum
    /// aliases to canonicalize, so the raw-spelling list change remains.
    #[test]
    fn update_string_list_of_enum_with_empty_schema_uses_exact_comparison() {
        let from = State::existing(
            ResourceId::with_identity("x.Thing", "t"),
            [("modes".to_string(), enum_list(&["allow"]))]
                .into_iter()
                .collect(),
        );
        let to =
            Resource::new("x.Thing", "t").with_attribute("modes", enum_list(&["Allow", "Deny"]));
        let effect = Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
            from: Box::new(from),
            to: resolved(to),
            changed_attributes: vec!["modes".to_string()],
        };
        let rows = build_detail_rows(&effect, None, DetailLevel::Explicit, None, None);
        assert_eq!(rows.len(), 1, "expected one row, got: {rows:?}");
        let DetailRow::StringListDiff { added, removed, .. } = &rows[0] else {
            panic!("expected StringListDiff, got: {rows:?}");
        };
        // Exact comparison: `allow` (state) is not in the new raw set
        // {`Allow`,`Deny`} → removed; both `Allow` and `Deny` are not
        // in the old raw set {`allow`} → added.
        assert_eq!(
            removed.iter().map(RenderedValue::plain).collect::<Vec<_>>(),
            vec!["allow"],
            "exact-comparison removal"
        );
        assert_eq!(
            added.iter().map(RenderedValue::plain).collect::<Vec<_>>(),
            vec!["Allow", "Deny"],
            "exact-comparison additions"
        );
    }
}
