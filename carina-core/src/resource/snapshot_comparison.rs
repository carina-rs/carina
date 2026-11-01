use super::*;
use super::SnapshotValue as Value;

#[derive(Clone, Copy)]
pub(crate) struct SavedValueViews<'a> {
    projected: &'a Value,
    raw: &'a Value,
}

impl<'a> SavedValueViews<'a> {
    pub(crate) fn new(projected: &'a Value, raw: &'a Value) -> Self {
        Self { projected, raw }
    }

    pub(crate) fn same(value: &'a Value) -> Self {
        Self::new(value, value)
    }

    pub(crate) fn projected(self) -> &'a Value {
        self.projected
    }
}

/// Borrow either concrete list representation at the comparison seam.
/// Conversion is local to comparison and never rewrites a payload or state.
/// Pair identical concrete items before probing other unordered candidates.
/// Preparation and comparison share this pairing so a reordered echoed
/// reference never requires resolution merely because another item came first.
pub(crate) fn match_equal_concrete_items<A: Borrow<Value>, B: Borrow<Value>>(
    a: &[A],
    b: &[B],
) -> (Vec<Option<usize>>, Vec<bool>) {
    let mut matched_a = vec![None; a.len()];
    let mut matched_b = vec![false; b.len()];
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, candidate) in b.iter().enumerate() {
        buckets
            .entry(candidate.borrow().canonical_hash())
            .or_default()
            .push(j);
    }
    for (i, item) in a.iter().enumerate() {
        let item = item.borrow();
        if !matches!(item, Value::Concrete(_)) || !item.to_value_at("comparison").is_ok_and(|value| value.is_fully_resolved()) {
            continue;
        }
        if let Some(j) = buckets.get(&item.canonical_hash()).and_then(|bucket| {
            bucket
                .iter()
                .copied()
                .find(|&j| !matched_b[j] && item == b[j].borrow())
        }) {
            matched_a[i] = Some(j);
            matched_b[j] = true;
        }
    }
    (matched_a, matched_b)
}

pub(crate) fn comparison_list_items(value: &Value) -> Option<std::borrow::Cow<'_, [Value]>> {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => Some(std::borrow::Cow::Borrowed(items)),
        Value::Concrete(ConcreteValue::StringList(items)) => Some(std::borrow::Cow::Owned(
            items
                .iter()
                .cloned()
                .map(ConcreteValue::String)
                .map(Value::Concrete)
                .collect(),
        )),
        _ => None,
    }
}

pub(crate) fn merge_with_saved<R: ReferenceEquality>(
    desired: &Value,
    saved: SavedValueViews<'_>,
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Value {
    match (desired, saved.projected) {
        (
            Value::Concrete(ConcreteValue::Map(desired_map)),
            Value::Concrete(ConcreteValue::Map(projected_saved_map)),
        ) => {
            let raw_saved_map = match saved.raw {
                Value::Concrete(ConcreteValue::Map(raw_saved_map)) => raw_saved_map,
                _ => projected_saved_map,
            };
            merge_maps(
                desired_map,
                projected_saved_map,
                raw_saved_map,
                prior_explicit,
                attr_type,
                defs,
                context,
            )
        }
        (
            Value::Concrete(ConcreteValue::List(desired_list)),
            Value::Concrete(ConcreteValue::List(projected_saved_list)),
        ) => {
            let raw_saved_list = match saved.raw {
                Value::Concrete(ConcreteValue::List(raw_saved_list)) => raw_saved_list,
                _ => projected_saved_list,
            };
            Value::Concrete(ConcreteValue::List(merge_lists(
                desired_list,
                projected_saved_list,
                raw_saved_list,
                prior_explicit,
                attr_type,
                defs,
                context,
            )))
        }
        _ => desired.clone(),
    }
}

fn merge_maps<R: ReferenceEquality>(
    desired: &IndexMap<String, Value>,
    projected_saved: &IndexMap<String, Value>,
    raw_saved: &IndexMap<String, Value>,
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Value {
    let prior_children = match prior_explicit {
        ExplicitFields::Struct { children } => Some(children),
        ExplicitFields::Unrecorded
        | ExplicitFields::Leaf
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::WholeValue { .. } => None,
    };

    let mut merged = projected_saved.clone();
    if let Some(children) = prior_children {
        merged.retain(|key, _| {
            let was_authored = children
                .get(key)
                .is_some_and(|child| !matches!(child, ExplicitFields::Unrecorded));
            desired.contains_key(key) || !was_authored
        });
    }
    for key in merged.keys() {
        if !desired.contains_key(key) {
            let paths = context.entry_paths(key, attr_type, defs);
            context.at_paths(&paths).copied_saved();
        }
    }
    for (key, desired_value) in desired {
        let merged_value = match projected_saved.get(key) {
            Some(projected_saved_value) => {
                let child_views = raw_saved
                    .get(key)
                    .map(|raw_saved_value| {
                        SavedValueViews::new(projected_saved_value, raw_saved_value)
                    })
                    .unwrap_or_else(|| SavedValueViews::same(projected_saved_value));
                merge_with_saved(
                    desired_value,
                    child_views,
                    prior_children
                        .and_then(|children| children.get(key))
                        .unwrap_or(&ExplicitFields::Unrecorded),
                    map_entry_subtype(attr_type, key, defs),
                    defs,
                    &context.at_paths(&context.entry_paths(key, attr_type, defs)),
                )
            }
            None => desired_value.clone(),
        };
        merged.insert(key.clone(), merged_value);
    }
    Value::Concrete(ConcreteValue::Map(merged))
}

/// Pair each desired-list index to at most one saved-list index.
///
/// Every positive-score candidate is computed before any element is consumed.
/// Candidates are then committed from highest score to lowest score. Equal
/// scores preserve stable list-order preferences when equivalent alternatives
/// exist. This keeps legacy assignments for indistinguishable peers without
/// allowing a lower score to beat a higher one. Accepting every positive score
/// preserves the existing score-1 fallback behavior.
///
/// Below [`HASH_THRESHOLD`] every pair is scored directly. Larger lists use
/// canonical-hash buckets to generate only pairs that can share an equal field
/// (or equal scalar), then run the exact same global best-first commit phase.
/// Hash collisions only add a candidate: [`similarity_score`] remains the
/// authority before a candidate can be committed.
fn pair_list_elements_snapshot<D, S, R: ReferenceEquality>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Vec<Option<usize>>
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    if !has_reference_leaves(attr_type, defs) {
        let candidates = list_pair_candidates(desired, saved, attr_type, defs, context);
        return commit_list_element_pairs(desired.len(), saved.len(), &candidates);
    }
    let (mut pairs, matched_saved) = match_equal_concrete_items(desired, saved);
    let desired_indices = pairs
        .iter()
        .enumerate()
        .filter_map(|(i, pair)| pair.is_none().then_some(i))
        .collect::<Vec<_>>();
    let saved_indices = matched_saved
        .iter()
        .enumerate()
        .filter_map(|(i, matched)| (!matched).then_some(i))
        .collect::<Vec<_>>();
    let remaining_desired = desired_indices
        .iter()
        .map(|&i| desired[i].borrow())
        .collect::<Vec<_>>();
    let remaining_saved = saved_indices
        .iter()
        .map(|&i| saved[i].borrow())
        .collect::<Vec<_>>();
    let candidates = list_pair_candidates(
        &remaining_desired,
        &remaining_saved,
        attr_type,
        defs,
        &context.with_indices(&context.remap_indices(&desired_indices, &saved_indices)),
    );
    for (i, pair) in
        commit_list_element_pairs(remaining_desired.len(), remaining_saved.len(), &candidates)
            .into_iter()
            .enumerate()
    {
        pairs[desired_indices[i]] = pair.map(|index| saved_indices[index]);
    }
    pairs
}

/// A stable list-element pairing together with the evidence for that pairing.
///
/// The ordinary differ separately uses a stable match for ambiguous peers as a
/// display/merge heuristic. State authoring transfer is not allowed to do so:
/// only [`Self::Unique`] establishes element identity. A zero-similarity
/// element is [`Self::Unmatched`] even when list position could supply a
/// tempting guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListElementPairing {
    Unique(usize),
    Ambiguous,
    Unmatched,
}

/// Pair list elements while distinguishing evidence-backed identity from the
/// stable tie-breaking used by the normal differ.
///
/// A match is unique only when the selected saved element is the sole
/// positive, highest-scoring candidate for that desired element. Global
/// contention that forces a lower-scoring choice is therefore ambiguous too.
fn pair_list_elements_with_uniqueness_snapshot<D, S, R: ReferenceEquality>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Vec<ListElementPairing>
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    if !has_reference_leaves(attr_type, defs) {
        let candidates = list_pair_candidates(desired, saved, attr_type, defs, context);
        let pairs = commit_list_element_pairs(desired.len(), saved.len(), &candidates);
        return classify_list_element_pairs(&pairs, saved.len(), &candidates);
    }
    let (exact, matched_saved) = match_equal_concrete_items(desired, saved);
    let desired_indices = exact
        .iter()
        .enumerate()
        .filter_map(|(i, pair)| pair.is_none().then_some(i))
        .collect::<Vec<_>>();
    let saved_indices = matched_saved
        .iter()
        .enumerate()
        .filter_map(|(i, matched)| (!matched).then_some(i))
        .collect::<Vec<_>>();
    let remaining_desired = desired_indices
        .iter()
        .map(|&i| desired[i].borrow())
        .collect::<Vec<_>>();
    let remaining_saved = saved_indices
        .iter()
        .map(|&i| saved[i].borrow())
        .collect::<Vec<_>>();
    let candidates = list_pair_candidates(
        &remaining_desired,
        &remaining_saved,
        attr_type,
        defs,
        &context.with_indices(&context.remap_indices(&desired_indices, &saved_indices)),
    );
    let pairs =
        commit_list_element_pairs(remaining_desired.len(), remaining_saved.len(), &candidates);
    let mut classified = vec![ListElementPairing::Unmatched; desired.len()];
    for (i, pairing) in classify_list_element_pairs(&pairs, remaining_saved.len(), &candidates)
        .into_iter()
        .enumerate()
    {
        classified[desired_indices[i]] = match pairing {
            ListElementPairing::Unique(index) => ListElementPairing::Unique(saved_indices[index]),
            other => other,
        };
    }
    for (i, value) in desired.iter().enumerate() {
        if let Some(index) = exact[i] {
            // Equal duplicates establish a stable merge pair, but cannot
            // establish a unique authoring correspondence.
            classified[i] = if desired
                .iter()
                .filter(|item| (*item).borrow() == value.borrow())
                .count()
                == 1
                && saved
                    .iter()
                    .filter(|item| (*item).borrow() == value.borrow())
                    .count()
                    == 1
            {
                ListElementPairing::Unique(index)
            } else {
                ListElementPairing::Ambiguous
            };
        } else if saved.iter().any(|item| item.borrow() == value.borrow()) {
            classified[i] = ListElementPairing::Ambiguous;
        }
    }
    classified
}

/// Reference-bearing list elements need D4 pre-pairing before similarity
/// probes. Keep ordinary list attribution and its score ordering unchanged.
fn has_reference_leaves(
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> bool {
    let mut pending = attr_type.into_iter().collect::<Vec<_>>();
    let mut seen = HashSet::new();
    while let Some(ty) = pending.pop() {
        if !seen.insert(ty as *const AttributeType) {
            continue;
        }
        match ty.shape_with_defs(defs) {
            Shape::Reference { .. } => return true,
            Shape::Struct { .. } => {
                if let Some(fields) = crate::schema::struct_fields_with_defs(ty, defs) {
                    pending.extend(fields.iter().map(|field| &field.field_type));
                }
            }
            Shape::List { element_type, .. } => pending.push(element_type),
            Shape::Map { value, .. } => pending.push(value),
            Shape::Union => {
                if let Some(members) = crate::schema::union_members_with_defs(ty, defs) {
                    pending.extend(members.iter().flatten().map(|member| member.as_attr()));
                }
            }
            _ => {}
        }
    }
    false
}

fn list_pair_candidates<D, S, R: ReferenceEquality>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    if desired.len().max(saved.len()) < HASH_THRESHOLD {
        pair_candidates_quadratic(desired, saved, attr_type, defs, context)
    } else {
        pair_candidates_hashed(desired, saved, attr_type, defs, context)
    }
}

fn commit_list_element_pairs(
    desired_len: usize,
    saved_len: usize,
    candidates: &PairCandidates,
) -> Vec<Option<usize>> {
    // Assign each desired element one stable, schema-aware preference before
    // committing anything. Preferences are not pair commits: every explicit
    // candidate and every implicit complete-row score has already been
    // computed, and score remains the first ordering key. Choosing an
    // unreserved equal-best saved index when possible is the O(n + m)
    // equivalent of the former desired×saved reservation matrix.
    let mut preferred_saved = vec![None; desired_len];
    let mut preferred_score = vec![0; desired_len];
    let mut preference_reserved = vec![false; saved_len];
    for (desired_index, preferred_slot) in preferred_saved.iter_mut().enumerate() {
        let entries = &candidates.entries[candidates.desired_offsets[desired_index]
            ..candidates.desired_offsets[desired_index + 1]];
        let best_score = entries
            .iter()
            .map(|(score, _, _)| *score)
            .max()
            .unwrap_or(0)
            .max(candidates.implicit_scores[desired_index]);
        if best_score == 0 {
            continue;
        }
        preferred_score[desired_index] = best_score;
        let preferred = if candidates.implicit_scores[desired_index] == best_score {
            // Every saved index has this score. Prefer the first index not
            // already reserved by an earlier desired element, matching the
            // explicit-candidate tie rule without materializing n×m edges.
            (0..saved_len)
                .find(|saved_index| !preference_reserved[*saved_index])
                .or((saved_len != 0).then_some(0))
        } else {
            let fallback = entries
                .iter()
                .filter(|(score, _, _)| *score == best_score)
                .map(|(_, _, saved_index)| *saved_index)
                .min();
            entries
                .iter()
                .filter(|(score, _, saved_index)| {
                    *score == best_score && !preference_reserved[*saved_index]
                })
                .map(|(_, _, saved_index)| *saved_index)
                .min()
                .or(fallback)
        };
        if let Some(saved_index) = preferred {
            *preferred_slot = Some(saved_index);
            preference_reserved[saved_index] = true;
        }
    }

    // Commit by the same total order as the former explicit edge sort:
    // descending score, preferred candidates first, then desired and saved
    // list order. Score levels let a complete row of equal-score candidates
    // stay compressed instead of allocating or visiting every edge.
    let mut score_levels = BTreeSet::new();
    score_levels.extend(candidates.entries.iter().map(|(score, _, _)| *score));
    score_levels.extend(
        candidates
            .implicit_scores
            .iter()
            .copied()
            .filter(|score| *score > 0),
    );
    let mut pairs = vec![None; desired_len];
    let mut available_saved: BTreeSet<usize> = (0..saved_len).collect();
    for score in score_levels.into_iter().rev() {
        // Preferred candidates sort before every non-preferred candidate at
        // the same score.
        for desired_index in 0..desired_len {
            if pairs[desired_index].is_some() || preferred_score[desired_index] != score {
                continue;
            }
            let Some(saved_index) = preferred_saved[desired_index] else {
                continue;
            };
            if available_saved.remove(&saved_index) {
                pairs[desired_index] = Some(saved_index);
            }
        }

        // Non-preferred candidates sort by desired index and then saved index.
        for (desired_index, pair) in pairs.iter_mut().enumerate() {
            if pair.is_some() {
                continue;
            }
            let saved_index = if candidates.implicit_scores[desired_index] == score {
                // A complete-row score means every still-available saved edge
                // has at least this score. If one had a higher score, the
                // explicit candidate would already have consumed one of its
                // endpoints in an earlier score phase. Therefore the first
                // available saved index is exactly the next edge in the old
                // global sort order.
                available_saved.first().copied()
            } else {
                candidates.entries[candidates.desired_offsets[desired_index]
                    ..candidates.desired_offsets[desired_index + 1]]
                    .iter()
                    .find_map(|(candidate_score, _, saved_index)| {
                        (*candidate_score == score && available_saved.contains(saved_index))
                            .then_some(*saved_index)
                    })
            };
            if let Some(saved_index) = saved_index {
                available_saved.remove(&saved_index);
                *pair = Some(saved_index);
            }
        }
    }
    pairs
}

fn classify_list_element_pairs(
    pairs: &[Option<usize>],
    saved_len: usize,
    candidates: &PairCandidates,
) -> Vec<ListElementPairing> {
    pairs
        .iter()
        .copied()
        .enumerate()
        .map(|(desired_index, selected)| {
            let entries = &candidates.entries[candidates.desired_offsets[desired_index]
                ..candidates.desired_offsets[desired_index + 1]];
            let explicit_best = entries
                .iter()
                .map(|(score, _, _)| *score)
                .max()
                .unwrap_or(0);
            let implicit_best = candidates.implicit_scores[desired_index];
            let best_score = explicit_best.max(implicit_best);

            if best_score == 0 {
                return ListElementPairing::Unmatched;
            }

            let sole_best = if implicit_best == best_score {
                (saved_len == 1).then_some(0)
            } else {
                let mut best_indices = entries
                    .iter()
                    .filter(|(score, _, _)| *score == best_score)
                    .map(|(_, _, saved_index)| *saved_index);
                let first = best_indices.next();
                match (first, best_indices.next()) {
                    (Some(index), None) => Some(index),
                    (None, None) | (Some(_), Some(_)) | (None, Some(_)) => None,
                }
            };

            match (selected, sole_best) {
                (Some(selected), Some(sole)) if selected == sole => {
                    ListElementPairing::Unique(selected)
                }
                (Some(_), Some(_) | None) | (None, Some(_) | None) => ListElementPairing::Ambiguous,
            }
        })
        .collect()
}

type PairCandidate = (usize, usize, usize);

struct PairCandidates {
    entries: Vec<PairCandidate>,
    desired_offsets: Vec<usize>,
    /// Per desired index, a positive score shared by every saved index.
    /// Explicit entries override it where selective fields add more matches.
    implicit_scores: Vec<usize>,
}

fn pair_candidates_quadratic<D, S, R: ReferenceEquality>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let mut entries = Vec::new();
    let mut desired_offsets = Vec::with_capacity(desired.len() + 1);
    desired_offsets.push(0);
    for (desired_index, desired_value) in desired.iter().enumerate() {
        for (saved_index, saved_value) in saved.iter().enumerate() {
            let score = similarity_score(
                desired_value.borrow(),
                saved_value.borrow(),
                attr_type,
                defs,
                &context.at_paths(&context.index_paths(desired_index, saved_index)),
            );
            if score > 0 {
                entries.push((score, desired_index, saved_index));
            }
        }
        desired_offsets.push(entries.len());
    }
    PairCandidates {
        entries,
        desired_offsets,
        implicit_scores: vec![0; desired.len()],
    }
}

/// Generate a complete representation of the positive-score candidate set.
///
/// Map elements are indexed by `(field key, schema-aware field hash)`, so a
/// desired/saved pair is scored only if at least one field could compare equal.
/// Non-map elements are indexed by their schema-aware whole-value hashes.
/// A field proven equal across every saved element is stored once as an
/// implicit complete-row contribution rather than expanding its bucket for
/// every desired element. Selective fields still produce explicit, exact
/// scores. Thus all candidates are scored before commitment while ubiquitous
/// values such as IAM `effect: "Allow"` do not create n×m entries.
///
/// Coarse fallback hashes for structural schema shapes may admit collisions,
/// but never exclude a pair that [`schema_aware_equal`] could accept. A dense
/// but non-universal bucket can still make the worst case quadratic; the
/// universal-field compression covers the common policy-statement shape while
/// preserving exactly the same pairing decisions.
fn pair_candidates_hashed<D, S, R: ReferenceEquality>(
    desired: &[D],
    saved: &[S],
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> PairCandidates
where
    D: Borrow<Value>,
    S: Borrow<Value>,
{
    let mut map_field_buckets: HashMap<&str, HashMap<u64, Vec<usize>>> = HashMap::new();
    let mut value_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (saved_index, saved_value) in saved.iter().enumerate() {
        match saved_value.borrow() {
            Value::Concrete(ConcreteValue::Map(map)) => {
                for (key, value) in map {
                    let field_type = map_entry_subtype(attr_type, key, defs);
                    for value_hash in pairing_hashes(value, field_type, defs, context) {
                        map_field_buckets
                            .entry(key)
                            .or_default()
                            .entry(value_hash)
                            .or_default()
                            .push(saved_index);
                    }
                }
            }
            value => {
                for value_hash in pairing_hashes(value, attr_type, defs, context) {
                    value_buckets
                        .entry(value_hash)
                        .or_default()
                        .push(saved_index);
                }
            }
        }
    }

    // Identify hash buckets that really are one schema-aware equivalence class
    // spanning every saved element. The equality verification makes the
    // compression exact even when canonical hashes collide or enum spellings
    // reach the same schema-normalized hash.
    let mut universal_map_fields: HashMap<&str, HashMap<u64, &Value>> = HashMap::new();
    for (key, value_buckets) in &map_field_buckets {
        let field_type = map_entry_subtype(attr_type, key, defs);
        // Universal-bucket compression must not invent current/current
        // reference comparisons. Only unmatched cross-side operands are
        // prepared, and a field absent from desired needs no identity lookup.
        if has_reference_leaves(field_type, defs) {
            continue;
        }
        let Some(representative) = saved.first().and_then(|saved_value| {
            let Value::Concrete(ConcreteValue::Map(map)) = saved_value.borrow() else {
                return None;
            };
            map.get(*key)
        }) else {
            continue;
        };
        for (value_hash, bucket) in value_buckets {
            if bucket.len() != saved.len()
                || !bucket.iter().copied().eq(0..saved.len())
                || !saved.iter().enumerate().all(|(index, saved_value)| {
                    let Value::Concrete(ConcreteValue::Map(map)) = saved_value.borrow() else {
                        return false;
                    };
                    map.get(*key).is_some_and(|value| {
                        schema_aware_equal(
                            representative,
                            value,
                            field_type,
                            defs,
                            &context.at_path(
                                &context
                                    .at_path(&context.index_path(index))
                                    .entry_path(key, attr_type, defs),
                            ),
                        )
                    })
                })
            {
                continue;
            }
            universal_map_fields
                .entry(key)
                .or_default()
                .insert(*value_hash, representative);
        }
    }

    let mut universal_values = HashMap::new();
    if !has_reference_leaves(attr_type, defs)
        && let Some(representative) = saved.first().map(Borrow::borrow)
    {
        for (value_hash, bucket) in &value_buckets {
            if bucket.len() == saved.len()
                && bucket.iter().copied().eq(0..saved.len())
                && saved.iter().enumerate().all(|(index, saved_value)| {
                    schema_aware_equal(
                        representative,
                        saved_value.borrow(),
                        attr_type,
                        defs,
                        &context.at_path(&context.index_path(index)),
                    )
                })
            {
                universal_values.insert(*value_hash, representative);
            }
        }
    }

    let mut entries = Vec::new();
    let mut desired_offsets = Vec::with_capacity(desired.len() + 1);
    let mut implicit_scores = Vec::with_capacity(desired.len());
    desired_offsets.push(0);
    let mut possible_saved = Vec::new();
    for (desired_index, desired_value) in desired.iter().enumerate() {
        possible_saved.clear();
        let mut implicit_score = 0;
        match desired_value.borrow() {
            Value::Concrete(ConcreteValue::Map(map)) => {
                for (key, value) in map {
                    let field_type = map_entry_subtype(attr_type, key, defs);
                    let value_hashes = pairing_hashes(value, field_type, defs, context);
                    let field_is_universal =
                        universal_map_fields
                            .get(key.as_str())
                            .is_some_and(|universal_buckets| {
                                value_hashes.iter().any(|value_hash| {
                                    universal_buckets.get(value_hash).is_some_and(
                                        |representative| {
                                            schema_aware_equal(
                                                value,
                                                representative,
                                                field_type,
                                                defs,
                                                &context.at_path(
                                                    &context.entry_path(key, attr_type, defs),
                                                ),
                                            )
                                        },
                                    )
                                })
                            });
                    if field_is_universal {
                        implicit_score += 1;
                        continue;
                    }
                    if let Some(field_buckets) = map_field_buckets.get(key.as_str()) {
                        for value_hash in value_hashes {
                            if let Some(bucket) = field_buckets.get(&value_hash) {
                                possible_saved.extend(bucket.iter().copied());
                            }
                        }
                    }
                }
            }
            value => {
                let value_hashes = pairing_hashes(value, attr_type, defs, context);
                let value_is_universal = value_hashes.iter().any(|value_hash| {
                    universal_values
                        .get(value_hash)
                        .is_some_and(|representative| {
                            schema_aware_equal(value, representative, attr_type, defs, context)
                        })
                });
                if value_is_universal {
                    implicit_score = 1;
                } else {
                    for value_hash in value_hashes {
                        let Some(bucket) = value_buckets.get(&value_hash) else {
                            continue;
                        };
                        possible_saved.extend(bucket.iter().copied());
                    }
                }
            }
        }
        possible_saved.sort_unstable();
        possible_saved.dedup();
        for saved_index in possible_saved.iter().copied() {
            let score = similarity_score(
                desired_value.borrow(),
                saved[saved_index].borrow(),
                attr_type,
                defs,
                &context.at_paths(&context.index_paths(desired_index, saved_index)),
            );
            if score > 0 {
                entries.push((score, desired_index, saved_index));
            }
        }
        desired_offsets.push(entries.len());
        implicit_scores.push(implicit_score);
    }
    PairCandidates {
        entries,
        desired_offsets,
        implicit_scores,
    }
}

fn pairing_hashes<R: ReferenceEquality>(
    value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Vec<u64> {
    let mut hashes = vec![value.canonical_hash()];
    if value.secret_hash().is_some() { hashes.push(secret_state_pairing_hash()); }
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            // `type_aware_equal` checks Secret before schema dispatch. A
            // secret can equal either its plaintext inner String or its typed
            // persisted-state hash, so candidate
            // generation must expose both relations as well. Recursive inner
            // hashes cover plaintext (including schema normalizations); the
            // fixed tag deliberately admits every state-secret spelling for
            // exact verification by `similarity_score`.
            hashes.extend(pairing_hashes(inner, attr_type, defs, context));
            hashes.push(secret_state_pairing_hash());
        }
        // Arithmetic has its own canonical hash above. A nested Secret is
        // normalized to an outer Secret by the evaluator before comparison,
        // so operand hashes must not become candidate hashes for the result.
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {}
        _ => {}
    }
    if let Some(attr_type) = attr_type {
        append_schema_pairing_hashes(value, attr_type, defs, context, &mut hashes, 0);
    }
    hashes.sort_unstable();
    hashes.dedup();
    hashes
}

fn secret_state_pairing_hash() -> u64 {
    pairing_component_hash(0x08, &RedactedSecretHash::V2_MARKER_PREFIX)
}

fn append_schema_pairing_hashes<R: ReferenceEquality>(
    value: &Value,
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
    hashes: &mut Vec<u64>,
    depth: usize,
) {
    if depth == 16 {
        hashes.push(pairing_component_hash(0xff, &0_u8));
        return;
    }
    match attr_type.shape_with_defs(defs) {
        Shape::Reference { .. } => {
            // Resolved equality must also be considered by multiset pairing.
            hashes.push(pairing_component_hash(0x09, &0_u8));
        }
        Shape::String {
            identity, to_dsl, ..
        } => {
            if let Value::Concrete(ConcreteValue::String(text)) = value {
                hashes.push(pairing_component_hash(
                    0x01,
                    &context.canonical_string(text.expose_plaintext(), identity, to_dsl),
                ));
            }
        }
        Shape::Bool | Shape::Duration => {}
        Shape::Int { .. } | Shape::Float { .. } => {
            if let Some(hash) = numeric_pairing_hash(value) {
                hashes.push(hash);
            }
        }
        Shape::Enum {
            values,
            dsl_aliases,
            to_dsl,
            ..
        } => {
            let canonical = match value {
                Value::Concrete(ConcreteValue::String(text)) => {
                    Some(text.expose_plaintext().to_string())
                }
                Value::Concrete(ConcreteValue::EnumIdentifier(text)) => {
                    let valid_values: Vec<&str> =
                        values.into_iter().flatten().map(String::as_str).collect();
                    let dsl_map = DslMap::new(dsl_aliases, to_dsl);
                    Some(text.canonical_api_spelling(&valid_values, &dsl_map))
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                    Some(value.api_value().to_string())
                }
                _ => None,
            };
            if let Some(canonical) = canonical {
                hashes.push(pairing_component_hash(
                    0x02,
                    &enum_value_comparison_key(&canonical),
                ));
            }
        }
        Shape::List {
            ordered,
            element_type: _element_type,
            constraints: _constraints,
        } => {
            if let Value::Concrete(ConcreteValue::List(items)) = value {
                hashes.push(pairing_component_hash(0x03, &(ordered, items.len())));
            }
        }
        Shape::Map { .. } => {
            if let Value::Concrete(ConcreteValue::Map(map)) = value {
                let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
                keys.sort_unstable();
                hashes.push(pairing_component_hash(0x04, &(map.len(), keys)));
            }
        }
        Shape::Struct { .. } => {
            if matches!(value, Value::Concrete(ConcreteValue::Map(_))) {
                // Struct equality tolerates omitted default-valued fields, so
                // raw map hashes are not a complete equivalence key.
                hashes.push(pairing_component_hash(0x05, &0_u8));
            }
        }
        Shape::Union => {
            let members = crate::schema::union_members_with_defs(attr_type, defs)
                .expect("Shape::Union must expose union members internally");
            for member in members.iter().flatten() {
                append_schema_pairing_hashes(
                    value,
                    member.as_attr(),
                    defs,
                    context,
                    hashes,
                    depth + 1,
                );
            }
        }
    }
}

fn numeric_pairing_hash(value: &Value) -> Option<u64> {
    match value {
        Value::Concrete(ConcreteValue::Int(value)) => Some(pairing_component_hash(0x06, value)),
        Value::Concrete(ConcreteValue::Float(value)) => {
            let integer = *value as i64;
            if integer as f64 == *value {
                Some(pairing_component_hash(0x06, &integer))
            } else {
                Some(pairing_component_hash(0x07, &value.to_bits()))
            }
        }
        _ => None,
    }
}

fn pairing_component_hash<T: Hash>(tag: u8, value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tag.hash(&mut hasher);
    value.hash(&mut hasher);
    hasher.finish()
}

/// Merge two lists using projected saved values for value pairing and raw saved values to
/// corroborate stored-index authoring attribution.
fn merge_lists<R: ReferenceEquality>(
    desired: &[Value],
    projected_saved: &[Value],
    raw_saved: &[Value],
    prior_explicit: &ExplicitFields,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> Vec<Value> {
    // `WholeValue` is the explicit whole-value fallback used when a carried row
    // cannot transfer a pending nested removal to a uniquely identified list
    // element. Merging individual saved elements here would fill the removed
    // child back in and hide the very diff the fallback represents.
    let stored_elements = match prior_explicit {
        ExplicitFields::WholeValue { .. } => return desired.to_vec(),
        ExplicitFields::ListElements { elements } if elements.len() == raw_saved.len() => {
            Some(elements)
        }
        ExplicitFields::Leaf
        | ExplicitFields::Struct { .. }
        | ExplicitFields::List { .. }
        | ExplicitFields::ListElements { .. }
        | ExplicitFields::Unrecorded => None,
    };

    let item_type = list_element_type(attr_type, defs);
    let projected_pairs = pair_list_elements(desired, projected_saved, item_type, defs, context);
    let raw_paths = context.raw_saved_paths();
    let raw_context = context.raw_saved_pair();
    let raw_context = raw_context.at_paths(&raw_paths);
    let raw_pairs = pair_list_elements(desired, raw_saved, item_type, defs, &raw_context);
    let projected_origins = if projected_saved.len() == raw_saved.len() {
        // Projection preserves order when it retains every stored element.
        // Keeping the direct index mapping also preserves the established
        // conservative behavior for indistinguishable duplicate peers.
        (0..projected_saved.len())
            .map(ListElementPairing::Unique)
            .collect()
    } else {
        let saved_paths = [context.right_path().into(), raw_paths[1].clone()];
        let saved_context = context.saved_pair();
        pair_list_elements_with_uniqueness(
            projected_saved,
            raw_saved,
            item_type,
            defs,
            &saved_context.at_paths(&saved_paths),
        )
    };

    projected_pairs
        .into_iter()
        .zip(raw_pairs)
        .enumerate()
        .map(
            |(desired_index, (projected_index, raw_index))| match projected_index {
                Some(projected_index) => {
                    // Writeback assigned `elements` using raw-list pairing, while the legacy value
                    // merge pairs against a projection that may have removed server-added
                    // elements. Resolve the projected element back to its stored index and require
                    // the independent raw pairing to corroborate that identity. Ambiguity or
                    // disagreement is uncertainty and must stay conservative.
                    let stored_index = match projected_origins[projected_index] {
                        ListElementPairing::Unique(stored_index)
                            if raw_index == Some(stored_index) =>
                        {
                            Some(stored_index)
                        }
                        ListElementPairing::Unique(_)
                        | ListElementPairing::Ambiguous
                        | ListElementPairing::Unmatched => None,
                    };
                    let authoring = stored_index
                        .and_then(|stored_index| {
                            stored_elements.map(|elements| &elements[stored_index])
                        })
                        .unwrap_or(&ExplicitFields::Unrecorded);
                    let projected_value = &projected_saved[projected_index];
                    let saved_views = stored_index
                        .and_then(|stored_index| raw_saved.get(stored_index))
                        .map(|raw_value| SavedValueViews::new(projected_value, raw_value))
                        .unwrap_or_else(|| SavedValueViews::same(projected_value));
                    merge_with_saved(
                        &desired[desired_index],
                        saved_views,
                        authoring,
                        item_type,
                        defs,
                        &context.at_paths(&context.index_paths(desired_index, projected_index)),
                    )
                }
                None => desired[desired_index].clone(),
            },
        )
        .collect()
}

/// Compute a similarity score between two Values.
/// For Maps: count matching key-value pairs.
/// For equal scalars: return 1.
/// Otherwise: return 0.
fn similarity_score<R: ReferenceEquality>(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    context: &ComparisonContext<'_, R>,
) -> usize {
    comparison_work::visit();
    match (a, b) {
        (Value::Concrete(ConcreteValue::Map(am)), Value::Concrete(ConcreteValue::Map(bm))) => am
            .iter()
            .filter(|(k, v)| {
                bm.get(*k)
                    .map(|bv| {
                        schema_aware_equal(
                            v,
                            bv,
                            map_entry_subtype(attr_type, k, defs),
                            defs,
                            &context.at_paths(&context.entry_paths(k, attr_type, defs)),
                        )
                    })
                    .unwrap_or(false)
            })
            .count(),
        _ => usize::from(schema_aware_equal(a, b, attr_type, defs, context)),
    }
}


pub(crate) trait ComparisonItem {
    fn comparison_snapshot(&self) -> super::SnapshotValue;
}
impl<P: StateValueLeaf> ComparisonItem for super::Value<P> {
    fn comparison_snapshot(&self) -> super::SnapshotValue { state_value_snapshot(self) }
}
impl<T: ComparisonItem + ?Sized> ComparisonItem for &T {
    fn comparison_snapshot(&self) -> super::SnapshotValue { (*self).comparison_snapshot() }
}

pub(crate) fn pair_list_elements<D: ComparisonItem, S: ComparisonItem, R: ReferenceEquality>(
    desired: &[D], saved: &[S], attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>, context: &ComparisonContext<'_,R>,
) -> Vec<Option<usize>> {
    let desired = desired.iter().map(ComparisonItem::comparison_snapshot).collect::<Vec<_>>();
    let saved = saved.iter().map(ComparisonItem::comparison_snapshot).collect::<Vec<_>>();
    pair_list_elements_snapshot(&desired, &saved, attr_type, defs, context)
}

pub(crate) fn pair_list_elements_with_uniqueness<D: ComparisonItem, S: ComparisonItem, R: ReferenceEquality>(
    desired: &[D], saved: &[S], attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>, context: &ComparisonContext<'_,R>,
) -> Vec<ListElementPairing> {
    let desired = desired.iter().map(ComparisonItem::comparison_snapshot).collect::<Vec<_>>();
    let saved = saved.iter().map(ComparisonItem::comparison_snapshot).collect::<Vec<_>>();
    pair_list_elements_with_uniqueness_snapshot(&desired, &saved, attr_type, defs, context)
}
