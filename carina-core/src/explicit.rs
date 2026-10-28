//! Per-resource user-authored field tree.
//!
//! `ExplicitFields` records which fields the user explicitly wrote in
//! their `.crn` for a resource. The differ projects the actual-state
//! through this tree before computing diffs so server-side default
//! fields the user never authored stop appearing as spurious removals.
//!
//! See `notes/specs/2026-05-10-explicit-fields-design.md`.

use crate::diff_helpers::{list_element_type, map_entry_subtype};
use crate::resource::{
    ConcreteValue, ListElementPairing, Resource, Value, pair_list_elements,
    pair_list_elements_with_uniqueness,
};
use crate::schema::{AttributeType, ResourceSchema, empty_defs_for_schema_walks};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Authored field paths whose removals could not be assigned to a specific
/// list element. Paths are relative to a list element and never contain a list
/// index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PendingPaths(BTreeSet<Vec<String>>);

impl PendingPaths {
    /// Construct a validated, non-empty set of non-empty field paths.
    pub fn try_new(paths: impl IntoIterator<Item = Vec<String>>) -> Option<Self> {
        let paths: BTreeSet<_> = paths.into_iter().collect();
        (!paths.is_empty()
            && paths
                .iter()
                .all(|path| !path.is_empty() && path.iter().all(|segment| !segment.is_empty())))
        .then_some(Self(paths))
    }

    fn iter(&self) -> impl Iterator<Item = &[String]> {
        self.0.iter().map(Vec::as_slice)
    }

    fn union(mut self, other: Self) -> Self {
        self.0.extend(other.0);
        self
    }
}

impl<'de> Deserialize<'de> for PendingPaths {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let paths = BTreeSet::<Vec<String>>::deserialize(deserializer)?;
        Self::try_new(paths).ok_or_else(|| {
            D::Error::custom(
                "pending paths must contain at least one non-empty field path with non-empty segments",
            )
        })
    }
}

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
/// - `WholeValue`: a carried list had a pending nested removal but its
///   element identity could not be established. The differ must compare
///   the entire list value instead of assigning authoring to a guessed
///   element. Normal desired-backed builders never produce this variant.
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
    /// A carried list must be compared as one authored value because pending
    /// nested removals could not be transferred to uniquely paired elements.
    /// The paths retain only authored removal evidence, never a guessed
    /// element identity.
    WholeValue {
        pending: PendingPaths,
    },
    /// No authoring record for this position. Top-level callers
    /// (`project_attributes`) treat this as "pass attrs through, the
    /// authoring shape is whatever the user wrote in `.crn` right
    /// now"; recursive `project` treats it the same as `Leaf` (keep
    /// the entire value). Used as the typed legacy-corruption marker
    /// when an older state migration encounters an empty `Struct`
    /// whose children may have been destroyed by an old write-path
    /// bug. A later desired-backed state write can seed it from current
    /// desired keys without treating provider-returned keys as authored.
    Unrecorded,
}

/// Build an `ExplicitFields::Struct` rooted at a resource's top-level
/// attributes. Lists are recorded in the resource value's order; state
/// writeback must use [`build_from_resource_for_stored_values`] instead.
#[cfg(test)]
pub(crate) fn build_from_resource(resource: &Resource) -> ExplicitFields {
    ExplicitFields::Struct {
        children: resource
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), build_from_value(v)))
            .collect(),
    }
}

/// Build a resource authoring tree whose list records are aligned to
/// the provider-returned values that will be stored in the same row.
///
/// When `schema` is present, list similarity and every recursive field walk
/// use the same schema-aware equality as differ-side saved-value merging. This
/// keeps the projected/raw corroboration gate aligned with the authoring index
/// selected during writeback.
///
/// Desired lists without a concrete stored-list counterpart fall back
/// to the legacy union [`ExplicitFields::List`] representation rather
/// than recording indices that cannot be justified.
pub fn build_from_resource_for_stored_values(
    resource: &Resource,
    stored_attributes: &HashMap<String, Value>,
    schema: Option<&ResourceSchema>,
) -> ExplicitFields {
    let defs = schema
        .map(|schema| &schema.defs)
        .unwrap_or(empty_defs_for_schema_walks());
    ExplicitFields::Struct {
        children: resource
            .attributes
            .iter()
            .map(|(key, desired)| {
                (
                    key.clone(),
                    build_from_value_for_stored_value(
                        desired,
                        stored_attributes.get(key),
                        schema
                            .and_then(|schema| schema.attributes.get(key))
                            .map(|attribute| &attribute.attr_type),
                        defs,
                    ),
                )
            })
            .collect(),
    }
}

/// Rebuild a carried row's authoring tree against fresh provider values while
/// preserving nested removals that have not taken effect yet.
///
/// Struct children have stable key identity and are transferred recursively.
/// Per-element list authoring is transferred only across mutually unique
/// prior-to-fresh and desired-to-fresh pairings. If a possible pending nested
/// removal crosses an ambiguous list identity, the list becomes
/// [`ExplicitFields::WholeValue`]. That keeps the removal visible without assigning
/// it to a guessed element.
pub fn build_carried_from_resource_for_stored_values(
    resource: &Resource,
    prior_stored_attributes: &HashMap<String, Value>,
    prior_explicit: &ExplicitFields,
    fresh_stored_attributes: &HashMap<String, Value>,
    schema: Option<&ResourceSchema>,
) -> ExplicitFields {
    let defs = schema
        .map(|schema| &schema.defs)
        .unwrap_or(empty_defs_for_schema_walks());
    let rebuilt = build_from_resource_for_stored_values(resource, fresh_stored_attributes, schema);
    let prior_children = match prior_explicit {
        ExplicitFields::Struct { children } => children,
        ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::WholeValue { .. }
        | ExplicitFields::Unrecorded => return rebuilt,
    };
    carry_struct_fields(
        prior_children,
        rebuilt,
        Some(CarryMap::Hash(prior_stored_attributes)),
        CarryMap::Index(&resource.attributes),
        CarryMap::Hash(fresh_stored_attributes),
        CarryContext::resource(schema, defs),
    )
    .explicit
}

struct CarriedNode {
    explicit: ExplicitFields,
    pending_removal: bool,
}

#[derive(Clone, Copy)]
enum CarryMap<'a> {
    Hash(&'a HashMap<String, Value>),
    Index(&'a IndexMap<String, Value>),
}

impl<'a> CarryMap<'a> {
    fn get(self, key: &str) -> Option<&'a Value> {
        match self {
            Self::Hash(fields) => fields.get(key),
            Self::Index(fields) => fields.get(key),
        }
    }
}

#[derive(Clone, Copy)]
enum CarryLocation<'a> {
    Resource(Option<&'a ResourceSchema>),
    Value(Option<&'a AttributeType>),
}

#[derive(Clone, Copy)]
struct CarryContext<'a> {
    location: CarryLocation<'a>,
    defs: &'a BTreeMap<String, AttributeType>,
}

impl<'a> CarryContext<'a> {
    fn resource(
        schema: Option<&'a ResourceSchema>,
        defs: &'a BTreeMap<String, AttributeType>,
    ) -> Self {
        Self {
            location: CarryLocation::Resource(schema),
            defs,
        }
    }

    fn value(
        attr_type: Option<&'a AttributeType>,
        defs: &'a BTreeMap<String, AttributeType>,
    ) -> Self {
        Self {
            location: CarryLocation::Value(attr_type),
            defs,
        }
    }

    fn child(self, key: &str) -> Self {
        let attr_type = match self.location {
            CarryLocation::Resource(schema) => schema
                .and_then(|schema| schema.attributes.get(key))
                .map(|attribute| &attribute.attr_type),
            CarryLocation::Value(attr_type) => map_entry_subtype(attr_type, key, self.defs),
        };
        Self::value(attr_type, self.defs)
    }

    fn list_element(self) -> Self {
        let attr_type = match self.location {
            CarryLocation::Resource(_) => None,
            CarryLocation::Value(attr_type) => list_element_type(attr_type, self.defs),
        };
        Self::value(attr_type, self.defs)
    }

    fn attr_type(self) -> Option<&'a AttributeType> {
        match self.location {
            CarryLocation::Resource(_) => None,
            CarryLocation::Value(attr_type) => attr_type,
        }
    }
}

fn carry_node(
    prior_explicit: &ExplicitFields,
    rebuilt: ExplicitFields,
    prior_stored: Option<&Value>,
    desired: &Value,
    fresh: &Value,
    context: CarryContext<'_>,
) -> CarriedNode {
    match prior_explicit {
        ExplicitFields::Leaf | ExplicitFields::Unrecorded => CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        },
        ExplicitFields::List { element } => {
            carry_legacy_list(element, rebuilt, desired, fresh, context)
        }
        ExplicitFields::WholeValue { pending } => {
            carry_whole_value(rebuilt, pending, desired, fresh, context)
        }
        ExplicitFields::Struct { children } => {
            carry_struct(children, rebuilt, prior_stored, desired, fresh, context)
        }
        ExplicitFields::ListElements { elements } => {
            carry_list(elements, rebuilt, prior_stored, desired, fresh, context)
        }
    }
}

fn carry_legacy_list(
    element: &ExplicitFields,
    rebuilt: ExplicitFields,
    desired: &Value,
    fresh: &Value,
    context: CarryContext<'_>,
) -> CarriedNode {
    let (Some(desired_items), Some(fresh_items)) = (concrete_list(desired), concrete_list(fresh))
    else {
        return CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        };
    };
    let (mut pending_by_element, _) = pending_paths_by_element(
        std::slice::from_ref(element),
        desired_items,
        fresh_items,
        context.list_element(),
    );
    let pending = pending_by_element.pop().and_then(PendingPaths::try_new);
    if let Some(pending) = pending {
        CarriedNode {
            explicit: ExplicitFields::WholeValue { pending },
            pending_removal: true,
        }
    } else {
        CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        }
    }
}

fn carry_whole_value(
    rebuilt: ExplicitFields,
    pending: &PendingPaths,
    desired: &Value,
    fresh: &Value,
    context: CarryContext<'_>,
) -> CarriedNode {
    let (Some(desired_items), Some(fresh_items)) = (concrete_list(desired), concrete_list(fresh))
    else {
        return CarriedNode {
            explicit: ExplicitFields::WholeValue {
                pending: pending.clone(),
            },
            pending_removal: true,
        };
    };
    // WholeValue no longer has per-element authoring records. Its payload can
    // remain pending only while the live list contains more occurrences of a
    // path than the desired list authors.
    let live_path_counts = pending
        .iter()
        .map(|path| {
            (
                path.to_vec(),
                fresh_items
                    .iter()
                    .filter(|item| field_path_is_present(item, path))
                    .count(),
            )
        })
        .collect();
    let (still_pending, _) = pending_candidate_paths(
        live_path_counts,
        desired_items,
        fresh_items,
        context.list_element(),
    );
    let still_present = PendingPaths::try_new(still_pending);
    if let Some(pending) = still_present {
        CarriedNode {
            explicit: ExplicitFields::WholeValue { pending },
            pending_removal: true,
        }
    } else {
        CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        }
    }
}

fn carry_struct(
    prior_children: &HashMap<String, ExplicitFields>,
    rebuilt: ExplicitFields,
    prior_stored: Option<&Value>,
    desired: &Value,
    fresh: &Value,
    context: CarryContext<'_>,
) -> CarriedNode {
    let (Some(desired_fields), Some(fresh_fields)) = (concrete_map(desired), concrete_map(fresh))
    else {
        return CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        };
    };
    carry_struct_fields(
        prior_children,
        rebuilt,
        prior_stored.and_then(concrete_map).map(CarryMap::Index),
        CarryMap::Index(desired_fields),
        CarryMap::Index(fresh_fields),
        context,
    )
}

fn carry_struct_fields(
    prior_children: &HashMap<String, ExplicitFields>,
    rebuilt: ExplicitFields,
    prior_fields: Option<CarryMap<'_>>,
    desired_fields: CarryMap<'_>,
    fresh_fields: CarryMap<'_>,
    context: CarryContext<'_>,
) -> CarriedNode {
    let mut rebuilt_children = match rebuilt {
        ExplicitFields::Struct { children } => children,
        other @ (ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::WholeValue { .. }
        | ExplicitFields::Unrecorded) => {
            return CarriedNode {
                explicit: other,
                pending_removal: false,
            };
        }
    };
    let mut pending_removal = false;

    for (key, prior_child) in prior_children {
        match (desired_fields.get(key), fresh_fields.get(key)) {
            (Some(desired_child), Some(fresh_child)) => {
                let Some(rebuilt_child) = rebuilt_children.remove(key) else {
                    continue;
                };
                let carried = carry_node(
                    prior_child,
                    rebuilt_child,
                    prior_fields.and_then(|fields| fields.get(key)),
                    desired_child,
                    fresh_child,
                    context.child(key),
                );
                pending_removal |= carried.pending_removal;
                rebuilt_children.insert(key.clone(), carried.explicit);
            }
            (None, Some(_)) => {
                pending_removal = true;
                rebuilt_children.insert(key.clone(), demote_list_elements_to_union(prior_child));
            }
            (Some(_), None) | (None, None) => {}
        }
    }

    CarriedNode {
        explicit: ExplicitFields::Struct {
            children: rebuilt_children,
        },
        pending_removal,
    }
}

fn carry_list(
    prior_elements: &[ExplicitFields],
    rebuilt: ExplicitFields,
    prior_stored: Option<&Value>,
    desired: &Value,
    fresh: &Value,
    context: CarryContext<'_>,
) -> CarriedNode {
    let (Some(desired_items), Some(fresh_items)) = (concrete_list(desired), concrete_list(fresh))
    else {
        return CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        };
    };
    // Compute candidate paths and desired/fresh identity evidence once. If
    // no path remains pending, the rebuilt record is authoritative and no
    // prior-list pairing is necessary.
    let item_context = context.list_element();
    let (candidate_paths, desired_by_fresh) =
        pending_paths_by_element(prior_elements, desired_items, fresh_items, item_context);
    if candidate_paths.iter().all(BTreeSet::is_empty) {
        return CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        };
    }

    let item_type = item_context.attr_type();
    let Some(prior_items) = prior_stored.and_then(concrete_list) else {
        return carried_list_without_alignment(&candidate_paths, rebuilt);
    };
    if prior_elements.len() != prior_items.len() {
        return carried_list_without_alignment(&candidate_paths, rebuilt);
    }

    let prior_to_desired =
        pair_list_elements_with_uniqueness(prior_items, desired_items, item_type, context.defs);
    let prior_to_fresh = mutually_unique_pairs(prior_items, fresh_items, item_type, context.defs);
    // The normal builder deliberately retains only the stable stored-index
    // authoring vector (including deterministic choices for duplicates). It
    // does not retain the desired index or uniqueness evidence, so transfer
    // must ask the shared pairing engine for that evidence here rather than
    // reverse-engineering identity from equal-looking authoring subtrees.
    let mut rebuilt_elements = match rebuilt {
        ExplicitFields::ListElements { elements } => elements,
        other @ (ExplicitFields::Leaf
        | ExplicitFields::Struct { .. }
        | ExplicitFields::List { .. }
        | ExplicitFields::WholeValue { .. }
        | ExplicitFields::Unrecorded) => {
            return CarriedNode {
                explicit: other,
                pending_removal: false,
            };
        }
    };
    let mut pending_removal = false;
    let mut unresolved_pending = BTreeSet::new();

    for (prior_index, prior_element) in prior_elements.iter().enumerate() {
        let uniquely_aligned = prior_to_fresh[prior_index].and_then(|fresh_index| {
            desired_by_fresh[fresh_index].map(|desired_index| (desired_index, fresh_index))
        });
        if let Some((desired_index, fresh_index)) = uniquely_aligned {
            let carried = carry_node(
                prior_element,
                rebuilt_elements[fresh_index].clone(),
                Some(&prior_items[prior_index]),
                &desired_items[desired_index],
                &fresh_items[fresh_index],
                item_context,
            );
            if carried.pending_removal {
                pending_removal = true;
                rebuilt_elements[fresh_index] = carried.explicit;
            }
        } else if matches!(
            prior_to_desired.get(prior_index),
            Some(ListElementPairing::Unique(_) | ListElementPairing::Ambiguous)
        ) {
            unresolved_pending.extend(candidate_paths[prior_index].iter().cloned());
        }
    }

    if let Some(pending) = PendingPaths::try_new(unresolved_pending) {
        return CarriedNode {
            explicit: ExplicitFields::WholeValue { pending },
            pending_removal: true,
        };
    }

    CarriedNode {
        explicit: ExplicitFields::ListElements {
            elements: rebuilt_elements,
        },
        pending_removal,
    }
}

fn carried_list_without_alignment(
    candidate_paths: &[BTreeSet<Vec<String>>],
    rebuilt: ExplicitFields,
) -> CarriedNode {
    let pending = candidate_paths
        .iter()
        .flat_map(|paths| paths.iter().cloned())
        .collect::<BTreeSet<_>>();
    if let Some(pending) = PendingPaths::try_new(pending) {
        CarriedNode {
            explicit: ExplicitFields::WholeValue { pending },
            pending_removal: true,
        }
    } else {
        CarriedNode {
            explicit: rebuilt,
            pending_removal: false,
        }
    }
}

fn mutually_unique_pairs(
    left: &[Value],
    right: &[Value],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> Vec<Option<usize>> {
    let forward = pair_list_elements_with_uniqueness(left, right, attr_type, defs);
    let reverse = pair_list_elements_with_uniqueness(right, left, attr_type, defs);
    forward
        .into_iter()
        .enumerate()
        .map(|(left_index, pairing)| match pairing {
            ListElementPairing::Unique(right_index) => match reverse.get(right_index) {
                Some(ListElementPairing::Unique(reverse_index)) if *reverse_index == left_index => {
                    Some(right_index)
                }
                Some(ListElementPairing::Unique(_))
                | Some(ListElementPairing::Ambiguous)
                | Some(ListElementPairing::Unmatched)
                | None => None,
            },
            ListElementPairing::Ambiguous | ListElementPairing::Unmatched => None,
        })
        .collect()
}

fn pending_paths_by_element(
    elements: &[ExplicitFields],
    desired_items: &[Value],
    fresh_items: &[Value],
    item_context: CarryContext<'_>,
) -> (Vec<BTreeSet<Vec<String>>>, Vec<Option<usize>>) {
    let authored_by_element: Vec<_> = elements
        .iter()
        .map(|element| {
            let mut paths = BTreeSet::new();
            collect_authored_paths(element, &mut Vec::new(), &mut paths);
            paths
        })
        .collect();
    let mut authored_counts = BTreeMap::new();
    for path in authored_by_element.iter().flatten() {
        *authored_counts.entry(path.clone()).or_insert(0) += 1;
    }
    let (candidates, desired_by_fresh) =
        pending_candidate_paths(authored_counts, desired_items, fresh_items, item_context);

    (
        authored_by_element
            .into_iter()
            .map(|paths| paths.intersection(&candidates).cloned().collect())
            .collect(),
        desired_by_fresh,
    )
}

fn pending_candidate_paths(
    prior_authored_counts: BTreeMap<Vec<String>, usize>,
    desired_items: &[Value],
    fresh_items: &[Value],
    item_context: CarryContext<'_>,
) -> (BTreeSet<Vec<String>>, Vec<Option<usize>>) {
    let broad_candidates: BTreeSet<_> = prior_authored_counts
        .into_iter()
        .filter_map(|(path, prior_authored_count)| {
            let desired_authored_count = desired_items
                .iter()
                .filter(|item| field_path_is_present(item, &path))
                .count();
            let some_fresh_item_has_path = fresh_items
                .iter()
                .any(|item| field_path_is_present(item, &path));
            (prior_authored_count > desired_authored_count && some_fresh_item_has_path)
                .then_some(path)
        })
        .collect();

    if broad_candidates.is_empty() {
        return (BTreeSet::new(), Vec::new());
    }

    let desired_by_fresh = mutually_unique_pairs(
        fresh_items,
        desired_items,
        item_context.attr_type(),
        item_context.defs,
    );
    let pending = broad_candidates
        .into_iter()
        .filter(|path| {
            fresh_items
                .iter()
                .enumerate()
                .filter(|(_, fresh_item)| field_path_is_present(fresh_item, path))
                .any(|(fresh_index, _)| {
                    desired_by_fresh[fresh_index].is_none_or(|desired_index| {
                        !field_path_is_present(&desired_items[desired_index], path)
                    })
                })
        })
        .collect::<BTreeSet<_>>();

    // With count evidence for an authored removal, identity uncertainty
    // resolves toward a visible retry. Indistinguishable duplicate identities
    // and reorder plus mutation can still require WholeValue because guessing
    // an element could silently drop a removal. A zero-similarity replacement
    // is Unmatched instead: its whole-element change is already visible to the
    // normal list diff as a removal/addition.
    //
    // Known boundary: removing P from one ambiguous element while adding P to
    // another in the same failed update leaves the authored counts equal, so
    // this deliberately does not infer a pending removal.
    (pending, desired_by_fresh)
}

fn collect_authored_paths(
    explicit: &ExplicitFields,
    prefix: &mut Vec<String>,
    paths: &mut BTreeSet<Vec<String>>,
) {
    match explicit {
        ExplicitFields::Leaf => {
            if !prefix.is_empty() {
                paths.insert(prefix.clone());
            }
        }
        ExplicitFields::Struct { children } => {
            let before = paths.len();
            for (key, child) in children {
                if matches!(child, ExplicitFields::Unrecorded) {
                    continue;
                }
                prefix.push(key.clone());
                collect_authored_paths(child, prefix, paths);
                prefix.pop();
            }
            if paths.len() == before && children.is_empty() && !prefix.is_empty() {
                paths.insert(prefix.clone());
            }
        }
        ExplicitFields::List { element } => {
            let before = paths.len();
            collect_authored_paths(element, prefix, paths);
            if paths.len() == before && !prefix.is_empty() {
                paths.insert(prefix.clone());
            }
        }
        ExplicitFields::ListElements { elements } => {
            let before = paths.len();
            for element in elements {
                collect_authored_paths(element, prefix, paths);
            }
            if paths.len() == before && !prefix.is_empty() {
                paths.insert(prefix.clone());
            }
        }
        ExplicitFields::WholeValue { pending } => {
            for path in pending.iter() {
                let mut full_path = prefix.clone();
                full_path.extend_from_slice(path);
                paths.insert(full_path);
            }
        }
        ExplicitFields::Unrecorded => {}
    }
}

fn field_path_is_present(value: &Value, path: &[String]) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return true;
    };
    match value {
        Value::Concrete(ConcreteValue::Map(fields)) => fields
            .get(head)
            .is_some_and(|child| field_path_is_present(child, tail)),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(|item| field_path_is_present(item, path))
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(_) => false,
    }
}

fn concrete_map(value: &Value) -> Option<&IndexMap<String, Value>> {
    if let Value::Concrete(ConcreteValue::Map(fields)) = value {
        Some(fields)
    } else {
        None
    }
}

fn concrete_list(value: &Value) -> Option<&[Value]> {
    if let Value::Concrete(ConcreteValue::List(items)) = value {
        Some(items)
    } else {
        None
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

fn build_from_value_for_stored_value(
    desired: &Value,
    stored: Option<&Value>,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> ExplicitFields {
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
                                map_entry_subtype(attr_type, key, defs),
                                defs,
                            ),
                        )
                    })
                    .collect(),
            }
        }
        Value::Concrete(ConcreteValue::List(desired_items)) => match stored {
            Some(Value::Concrete(ConcreteValue::List(stored_items))) => {
                let item_type = list_element_type(attr_type, defs);
                let mut elements = vec![ExplicitFields::Unrecorded; stored_items.len()];
                for (desired_index, stored_index) in
                    pair_list_elements(desired_items, stored_items, item_type, defs)
                        .into_iter()
                        .enumerate()
                {
                    if let Some(stored_index) = stored_index {
                        elements[stored_index] = build_from_value_for_stored_value(
                            &desired_items[desired_index],
                            Some(&stored_items[stored_index]),
                            item_type,
                            defs,
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
        ExplicitFields::WholeValue { pending } => ExplicitFields::WholeValue {
            pending: pending.clone(),
        },
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
        (WholeValue { pending: a }, WholeValue { pending: b }) => WholeValue {
            pending: a.union(b),
        },
        (
            whole @ WholeValue { .. },
            Leaf | Unrecorded | Struct { .. } | List { .. } | ListElements { .. },
        )
        | (
            Leaf | Unrecorded | Struct { .. } | List { .. } | ListElements { .. },
            whole @ WholeValue { .. },
        ) => whole,
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
        (Leaf, Leaf) => Leaf,
        (Leaf, Unrecorded) | (Unrecorded, Leaf) | (Unrecorded, Unrecorded) => Unrecorded,
        (Leaf | Unrecorded, Struct { children }) => Struct { children },
        (Struct { children }, Leaf | Unrecorded) => Struct { children },
        (Leaf | Unrecorded, List { element }) => List { element },
        (List { element }, Leaf | Unrecorded) => List { element },
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
        (Struct { children }, List { .. }) => Struct { children },
        (List { element }, Struct { .. }) => List { element },
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
        // An ambiguous carried list is deliberately projected intact; its
        // whole-value merge rule, not projection, keeps the removal visible.
        ExplicitFields::WholeValue { .. } => value,
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
/// record — emitted by the carina#3280 legacy-state migration)
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
        // filtering. A later desired-backed state write can seed its
        // `Struct` from desired keys without promoting provider-only keys.
        ExplicitFields::Unrecorded => attrs,
        // Top-level Leaf / List / ListElements shouldn't occur for a
        // resource's full attribute set; pass through conservatively.
        ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::WholeValue { .. } => attrs,
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
                Value::Concrete(ConcreteValue::String(description.to_string().into())),
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
            build_from_resource_for_stored_values(&resource, &stored_attributes, None)
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

    fn whole_value(paths: &[&[&str]]) -> ExplicitFields {
        ExplicitFields::WholeValue {
            pending: PendingPaths::try_new(
                paths
                    .iter()
                    .map(|path| path.iter().map(|segment| (*segment).to_string()).collect()),
            )
            .expect("test pending paths are non-empty and valid"),
        }
    }

    fn carried_items_explicit(
        desired_items: Vec<Value>,
        prior_items: Vec<Value>,
        prior_explicit: &ExplicitFields,
        fresh_items: Vec<Value>,
    ) -> ExplicitFields {
        carried_items_explicit_with_schema(
            desired_items,
            prior_items,
            prior_explicit,
            fresh_items,
            None,
        )
    }

    fn carried_items_explicit_with_schema(
        desired_items: Vec<Value>,
        prior_items: Vec<Value>,
        prior_explicit: &ExplicitFields,
        fresh_items: Vec<Value>,
        schema: Option<&ResourceSchema>,
    ) -> ExplicitFields {
        let resource = Resource::new("example.Listener", "listener")
            .with_attribute("items", Value::Concrete(ConcreteValue::List(desired_items)));
        let prior_stored = HashMap::from([(
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(prior_items)),
        )]);
        let fresh_stored = HashMap::from([(
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(fresh_items)),
        )]);
        build_carried_from_resource_for_stored_values(
            &resource,
            &prior_stored,
            prior_explicit,
            &fresh_stored,
            schema,
        )
    }

    fn root_items_child(explicit: &ExplicitFields) -> &ExplicitFields {
        let ExplicitFields::Struct { children } = explicit else {
            panic!("expected resource-root Struct, got {explicit:?}");
        };
        &children["items"]
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
    fn carried_legacy_list_union_falls_back_for_a_pending_removal() {
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::List {
                    element: Box::new(ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    }),
                },
            )]),
        };
        let carried = carried_items_explicit(
            vec![rule(80, None), rule(443, None)],
            vec![rule(80, Some("web")), rule(443, None)],
            &prior_explicit,
            vec![rule(80, Some("web")), rule(443, None)],
        );

        assert_eq!(
            root_items_child(&carried),
            &whole_value(&[&["description"]])
        );
    }

    #[test]
    fn carried_legacy_list_union_mixed_list_without_removal_uses_the_rebuilt_record() {
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::List {
                    element: Box::new(ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    }),
                },
            )]),
        };
        let carried = carried_items_explicit(
            vec![rule(80, Some("web")), rule(443, None)],
            vec![rule(80, Some("web")), rule(443, None)],
            &prior_explicit,
            vec![rule(80, Some("web")), rule(443, None)],
        );

        assert!(matches!(
            root_items_child(&carried),
            ExplicitFields::ListElements { .. }
        ));
    }

    #[test]
    fn carried_list_without_prior_stored_mixed_list_without_removal_uses_rebuilt_record() {
        let desired_items = vec![rule(80, Some("web")), rule(443, None)];
        let resource = Resource::new("example.Listener", "listener").with_attribute(
            "items",
            Value::Concrete(ConcreteValue::List(desired_items.clone())),
        );
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::ListElements {
                    elements: vec![
                        ExplicitFields::Struct {
                            children: HashMap::from([
                                ("port".to_string(), ExplicitFields::Leaf),
                                ("description".to_string(), ExplicitFields::Leaf),
                            ]),
                        },
                        ExplicitFields::Struct {
                            children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                        },
                    ],
                },
            )]),
        };
        let fresh_stored = HashMap::from([(
            "items".to_string(),
            Value::Concrete(ConcreteValue::List(desired_items)),
        )]);

        let carried = build_carried_from_resource_for_stored_values(
            &resource,
            &HashMap::new(),
            &prior_explicit,
            &fresh_stored,
            None,
        );

        assert!(matches!(
            root_items_child(&carried),
            ExplicitFields::ListElements { .. }
        ));
    }

    #[test]
    fn carried_list_length_mismatch_mixed_list_without_removal_uses_rebuilt_record() {
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::ListElements {
                    elements: vec![
                        ExplicitFields::Struct {
                            children: HashMap::from([
                                ("port".to_string(), ExplicitFields::Leaf),
                                ("description".to_string(), ExplicitFields::Leaf),
                            ]),
                        },
                        ExplicitFields::Struct {
                            children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                        },
                    ],
                },
            )]),
        };
        let carried = carried_items_explicit(
            vec![rule(80, Some("web")), rule(443, None)],
            vec![rule(999, None)],
            &prior_explicit,
            vec![rule(80, Some("web")), rule(443, None)],
        );

        assert!(matches!(
            root_items_child(&carried),
            ExplicitFields::ListElements { .. }
        ));
    }

    #[test]
    fn carried_list_length_mismatch_keeps_every_pending_candidate() {
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::ListElements {
                    elements: vec![ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    }],
                },
            )]),
        };
        let carried = carried_items_explicit(
            vec![rule(80, None)],
            vec![rule(999, None), rule(80, Some("web"))],
            &prior_explicit,
            vec![rule(80, Some("web"))],
        );

        assert_eq!(
            root_items_child(&carried),
            &whole_value(&[&["description"]])
        );
    }

    #[test]
    fn carried_ambiguous_elements_track_path_missing_from_any_desired_peer() {
        let with_provider_default = |mut value: Value| {
            let Value::Concrete(ConcreteValue::Map(fields)) = &mut value else {
                unreachable!("test rule is a map");
            };
            fields.insert(
                "provider_default".to_string(),
                Value::Concrete(ConcreteValue::String("filled".into())),
            );
            value
        };
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([(
                "items".to_string(),
                ExplicitFields::ListElements {
                    elements: vec![
                        ExplicitFields::Struct {
                            children: HashMap::from([
                                ("port".to_string(), ExplicitFields::Leaf),
                                ("description".to_string(), ExplicitFields::Leaf),
                            ]),
                        },
                        ExplicitFields::Struct {
                            children: HashMap::from([
                                ("port".to_string(), ExplicitFields::Leaf),
                                ("description".to_string(), ExplicitFields::Leaf),
                            ]),
                        },
                    ],
                },
            )]),
        };
        let carried = carried_items_explicit(
            vec![rule(80, None), rule(80, Some("b"))],
            vec![
                with_provider_default(rule(80, Some("a"))),
                with_provider_default(rule(80, Some("b"))),
            ],
            &prior_explicit,
            vec![
                with_provider_default(rule(80, Some("a"))),
                with_provider_default(rule(80, Some("b"))),
            ],
        );

        assert_eq!(
            serde_json::to_value(root_items_child(&carried)).unwrap(),
            serde_json::json!({
                "kind": "whole-value",
                "pending": [["description"]]
            })
        );
    }

    #[test]
    fn whole_value_has_a_dedicated_serde_kind() {
        let json = r#"{"kind":"whole-value","pending":[["description"],["nested","tag"]]}"#;
        let parsed: Result<ExplicitFields, _> = serde_json::from_str(json);
        let parsed = parsed.expect("whole-value kind must deserialize");
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    #[test]
    fn whole_value_rejects_empty_pending_paths() {
        let parsed: Result<ExplicitFields, _> =
            serde_json::from_str(r#"{"kind":"whole-value","pending":[]}"#);
        assert!(parsed.is_err(), "an empty pending-path set is invalid");
    }

    #[test]
    fn merging_whole_values_unions_pending_paths() {
        let merged = merge(
            whole_value(&[&["description"]]),
            whole_value(&[&["nested", "tag"]]),
        );
        assert_eq!(
            serde_json::to_value(merged).unwrap(),
            serde_json::json!({
                "kind": "whole-value",
                "pending": [["description"], ["nested", "tag"]]
            })
        );
    }

    #[test]
    fn carried_whole_value_fallback_survives_refresh_until_values_converge() {
        use crate::schema::{AttributeSchema, StructField};

        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([("items".to_string(), whole_value(&[&["description"]]))]),
        };
        let schema = ResourceSchema::new("example.Listener").attribute(
            AttributeSchema::optional(
                "items",
                AttributeType::list(AttributeType::struct_(
                    "Rule",
                    vec![
                        StructField::optional("port", AttributeType::int()).build(),
                        StructField::optional("description", AttributeType::string()).build(),
                        StructField::provider_populated("provider_status", AttributeType::string())
                            .build(),
                    ],
                )),
            )
            .build(),
        );
        let desired = vec![rule(80, None), rule(80, None)];
        let with_provider_status = |mut value: Value| {
            let Value::Concrete(ConcreteValue::Map(fields)) = &mut value else {
                unreachable!("test rule is a map");
            };
            fields.insert(
                "provider_status".to_string(),
                Value::Concrete(ConcreteValue::String("ready".into())),
            );
            value
        };
        let still_live = vec![
            with_provider_status(rule(80, Some("web"))),
            with_provider_status(rule(80, None)),
        ];
        let carried = carried_items_explicit_with_schema(
            desired.clone(),
            still_live.clone(),
            &prior_explicit,
            still_live.clone(),
            Some(&schema),
        );
        assert_eq!(
            root_items_child(&carried),
            &whole_value(&[&["description"]])
        );

        let out_of_band = vec![
            with_provider_status(rule(80, None)),
            with_provider_status(rule(80, None)),
        ];
        let converged = carried_items_explicit_with_schema(
            desired,
            still_live,
            &carried,
            out_of_band,
            Some(&schema),
        );
        assert!(matches!(
            root_items_child(&converged),
            ExplicitFields::ListElements { .. }
        ));
    }

    #[test]
    fn carried_whole_value_clears_when_readded_path_has_unique_pairing() {
        let prior_explicit = ExplicitFields::Struct {
            children: HashMap::from([("items".to_string(), whole_value(&[&["description"]]))]),
        };
        let desired = vec![rule(80, Some("web")), rule(443, None)];
        let carried =
            carried_items_explicit(desired.clone(), desired.clone(), &prior_explicit, desired);

        assert!(matches!(
            root_items_child(&carried),
            ExplicitFields::ListElements { .. }
        ));
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
            Value::Concrete(ConcreteValue::String("two".into())),
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
                "one".into(),
                "two".into(),
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
    fn build_from_resource_records_all_resource_attributes() {
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
        assert!(children.contains_key("_internal"));
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
    fn aligned_builder_pairing_matches_schema_aware_differ_pairing() {
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

        let effect_type = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("Effect"),
            Some(vec!["Allow".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let item_type = AttributeType::struct_(
            "Statement",
            vec![
                StructField::optional("effect", effect_type).build(),
                StructField::optional("description", AttributeType::string()).build(),
            ],
        );
        let schema = ResourceSchema::new("example.Listener").attribute(
            AttributeSchema::optional("items", AttributeType::unordered_list(item_type)).build(),
        );
        let desired_item = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier("allow")),
            ),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("authored".into())),
            ),
        ])));
        let stored_item = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "effect".to_string(),
                Value::Concrete(ConcreteValue::String("Allow".into())),
            ),
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("provider".into())),
            ),
        ])));
        let desired = Value::Concrete(ConcreteValue::List(vec![desired_item]));
        let stored = Value::Concrete(ConcreteValue::List(vec![stored_item]));
        let resource =
            Resource::new("example.Listener", "listener").with_attribute("items", desired);
        let stored_attributes = HashMap::from([("items".to_string(), stored)]);

        let ExplicitFields::Struct { children } =
            build_from_resource_for_stored_values(&resource, &stored_attributes, Some(&schema))
        else {
            panic!("expected resource-root Struct");
        };
        let ExplicitFields::ListElements { elements } = &children["items"] else {
            panic!("expected stored-index-aligned ListElements");
        };
        let Value::Concrete(ConcreteValue::List(desired_items)) = &resource.attributes["items"]
        else {
            panic!("expected desired list");
        };
        let Value::Concrete(ConcreteValue::List(stored_items)) = &stored_attributes["items"] else {
            panic!("expected stored list");
        };
        let list_type = &schema.attributes["items"].attr_type;
        let item_type = list_element_type(Some(list_type), &schema.defs);
        let differ_pairs = pair_list_elements(desired_items, stored_items, item_type, &schema.defs);
        let writeback_paired_indices: Vec<usize> = elements
            .iter()
            .enumerate()
            .filter_map(|(index, element)| {
                (!matches!(element, ExplicitFields::Unrecorded)).then_some(index)
            })
            .collect();
        assert_eq!(differ_pairs, vec![Some(0)]);
        assert_eq!(writeback_paired_indices, vec![0]);
        assert_struct_keys(&elements[0], &["effect", "description"]);
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
                    Value::Concrete(ConcreteValue::String("unrelated".into())),
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
                "shape-mismatch".into(),
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
                ExplicitFields::Leaf
                | ExplicitFields::WholeValue { .. }
                | ExplicitFields::Unrecorded => false,
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
                    Value::Concrete(ConcreteValue::String(sentinel.to_string().into())),
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
                sentinel.expose_plaintext()
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
