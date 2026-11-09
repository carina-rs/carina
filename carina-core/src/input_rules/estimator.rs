//! A conservative abstract interpretation of the *retained* desired expression.
//! Each set/interval over-approximates possible strings. Mandatory facts are
//! under-approximations: forgetting facts can only turn a decision into pending.

use super::*;
use crate::{
    binding_index::{InvalidAccessPolicy, narrow_type},
    resource::{AccessPath, InterpolationPart, PathSegment, Subscript},
    schema::{
        AttrTypeKind, CharSet, CompiledAttributeType, RawShape, length_interval::LengthInterval,
    },
};

const MAX_WINDOW: usize = 64;
const MAX_ALTERNATIVES: usize = 128;
const MAX_STEPS: usize = 4096;
const MAX_DEPTH: usize = 64;

/// Sorted, coalesced possible Unicode scalars. An empty set represents no
/// possible character, whereas ALL is an unknown character.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Characters(Vec<(char, char)>);

impl Characters {
    fn all() -> Self {
        Self(vec![('\0', char::MAX)])
    }
    fn empty() -> Self {
        Self(Vec::new())
    }
    fn one(ch: char) -> Self {
        Self(vec![(ch, ch)])
    }
    fn from_set(set: &CharSet) -> Self {
        Self(set.ranges().to_vec())
    }
    fn contains(&self, ch: char) -> bool {
        self.0.iter().any(|(a, b)| *a <= ch && ch <= *b)
    }
    fn union(&self, other: &Self) -> Self {
        let ranges = self.0.iter().chain(&other.0).copied().collect::<Vec<_>>();
        if ranges.is_empty() {
            Self::empty()
        } else {
            Self::from_set(&CharSet::new(ranges).expect("valid scalar ranges"))
        }
    }
    fn subset(&self, other: &Self) -> bool {
        self.0
            .iter()
            .all(|(a, b)| other.0.iter().any(|(c, d)| c <= a && b <= d))
    }
    fn disjoint(&self, other: &Self) -> bool {
        !self
            .0
            .iter()
            .any(|(a, b)| other.0.iter().any(|(c, d)| a <= d && c <= b))
    }
}

type Window = Vec<Characters>;

/// A union of bounded symbolic prefix/suffix strings. Short windows denote
/// entire strings; a full window may be truncated. Including the empty window
/// is essential: joining across it skips any number of nullable parts.
#[derive(Clone)]
struct Windows(Vec<Window>);

impl Windows {
    fn homogeneous(length: LengthInterval, alphabet: &Characters, limit: usize) -> Self {
        let low = usize::try_from(length.min).unwrap_or(usize::MAX).min(limit);
        let high = usize::try_from(length.max.unwrap_or(u64::MAX))
            .unwrap_or(usize::MAX)
            .min(limit);
        Self((low..=high).map(|n| vec![alphabet.clone(); n]).collect())
    }

    fn join(
        &self,
        other: &Self,
        limit: usize,
        suffix: bool,
        length: LengthInterval,
        alphabet: &Characters,
    ) -> Self {
        if self.0.len().saturating_mul(other.0.len()) > MAX_ALTERNATIVES {
            return Self::homogeneous(length, alphabet, limit);
        }
        let mut windows = Vec::new();
        for a in &self.0 {
            for b in &other.0 {
                let joined = if suffix && b.len() == limit {
                    b.clone()
                } else if !suffix && a.len() == limit {
                    a.clone()
                } else {
                    let mut joined = a.iter().chain(b).cloned().collect::<Vec<_>>();
                    if suffix && joined.len() > limit {
                        joined.drain(..joined.len() - limit);
                    }
                    joined.truncate(limit);
                    joined
                };
                if !windows.contains(&joined) {
                    windows.push(joined);
                }
            }
        }
        Self(windows)
    }

    fn union(
        &self,
        other: &Self,
        length: LengthInterval,
        alphabet: &Characters,
        limit: usize,
    ) -> Self {
        if self.0.len() + other.0.len() > MAX_ALTERNATIVES {
            Self::homogeneous(length, alphabet, limit)
        } else {
            Self(self.0.iter().chain(&other.0).cloned().collect())
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Occurrence {
    possible: bool,
    mandatory: bool,
}

#[derive(Clone)]
struct Summary {
    length: LengthInterval,
    alphabet: Characters,
    first: Characters,
    last: Characters,
    prefix: Windows,
    suffix: Windows,
    occurrence: Occurrence,
    bad_character: bool,
    finite: Option<Vec<String>>,
    may_be_absent: bool,
    exact_keys: bool,
}

#[derive(Clone, Copy)]
struct Query<'a> {
    predicate: &'a InputPredicate<crate::schema::CompiledPattern>,
    limit: usize,
    supported_window: bool,
}

impl<'a> Query<'a> {
    fn new(predicate: &'a InputPredicate<crate::schema::CompiledPattern>) -> Self {
        use InputPredicate::*;
        let limit = match predicate {
            NoSubstring(token) => token.as_str().chars().count().saturating_sub(1),
            NoPrefix(token) | NoSuffix(token) => token.as_str().chars().count(),
            ForbiddenPrefix { prefix, .. } => prefix.as_str().chars().count(),
            NotIpv4 => 16,
            _ => 1,
        };
        Self {
            predicate,
            limit: limit.min(MAX_WINDOW),
            supported_window: limit <= MAX_WINDOW,
        }
    }
}

fn any_of(summaries: impl IntoIterator<Item = Summary>, query: &Query<'_>) -> Option<Summary> {
    summaries
        .into_iter()
        .reduce(|a, b| a.alternatives(b, query))
}

impl Summary {
    fn homogeneous(length: LengthInterval, alphabet: Characters, query: &Query<'_>) -> Self {
        let endpoints = if length.max == Some(0) {
            Characters::empty()
        } else {
            alphabet.clone()
        };
        let occurrence = match query.predicate {
            InputPredicate::NoSubstring(token) => {
                let chars = token.as_str().chars().collect::<Vec<_>>();
                let fits = length.max.is_none_or(|max| max >= chars.len() as u64);
                Occurrence {
                    possible: fits && chars.iter().all(|ch| alphabet.contains(*ch)),
                    mandatory: length.min >= chars.len() as u64
                        && chars
                            .iter()
                            .all(|ch| alphabet.subset(&Characters::one(*ch))),
                }
            }
            InputPredicate::NoAdjacent { left, right } => Occurrence {
                possible: length.max.is_none_or(|max| max >= 2)
                    && !alphabet.disjoint(&Characters::from_set(left))
                    && !alphabet.disjoint(&Characters::from_set(right)),
                mandatory: length.min >= 2
                    && alphabet.subset(&Characters::from_set(left))
                    && alphabet.subset(&Characters::from_set(right)),
            },
            _ => Occurrence::default(),
        };
        let bad_character = match query.predicate {
            InputPredicate::AllowedChars(set) => {
                length.min > 0 && alphabet.disjoint(&Characters::from_set(set))
            }
            _ => false,
        };
        Self {
            length,
            first: endpoints.clone(),
            last: endpoints,
            prefix: Windows::homogeneous(length, &alphabet, query.limit),
            suffix: Windows::homogeneous(length, &alphabet, query.limit),
            alphabet,
            occurrence,
            bad_character,
            finite: None,
            may_be_absent: false,
            exact_keys: false,
        }
    }

    fn unknown(query: &Query<'_>) -> Self {
        Self::homogeneous(LengthInterval::ANY, Characters::all(), query)
    }

    fn literal(value: &str, query: &Query<'_>) -> Self {
        let chars = value.chars().collect::<Vec<_>>();
        let alphabet = chars.iter().fold(Characters::empty(), |set, ch| {
            set.union(&Characters::one(*ch))
        });
        let mut summary =
            Self::homogeneous(LengthInterval::exact(chars.len() as u64), alphabet, query);
        summary.first = chars
            .first()
            .map_or_else(Characters::empty, |ch| Characters::one(*ch));
        summary.last = chars
            .last()
            .map_or_else(Characters::empty, |ch| Characters::one(*ch));
        summary.prefix = Windows(vec![
            chars
                .iter()
                .take(query.limit)
                .map(|ch| Characters::one(*ch))
                .collect(),
        ]);
        summary.suffix = Windows(vec![
            chars
                .iter()
                .skip(chars.len().saturating_sub(query.limit))
                .map(|ch| Characters::one(*ch))
                .collect(),
        ]);
        let found = match query.predicate {
            InputPredicate::NoSubstring(token) => value.contains(token.as_str()),
            InputPredicate::NoAdjacent { left, right } => chars
                .windows(2)
                .any(|pair| left.contains(pair[0]) && right.contains(pair[1])),
            _ => false,
        };
        summary.occurrence = Occurrence {
            possible: found,
            mandatory: found,
        };
        if let InputPredicate::AllowedChars(set) = query.predicate {
            summary.bad_character = chars.iter().any(|ch| !set.contains(*ch));
        }
        summary.finite = Some(vec![value.to_string()]);
        summary
    }

    fn alternatives(self, other: Self, query: &Query<'_>) -> Self {
        let length = self.length.hull(other.length);
        let alphabet = self.alphabet.union(&other.alphabet);
        Self {
            length,
            first: self.first.union(&other.first),
            last: self.last.union(&other.last),
            prefix: self
                .prefix
                .union(&other.prefix, length, &alphabet, query.limit),
            suffix: self
                .suffix
                .union(&other.suffix, length, &alphabet, query.limit),
            alphabet,
            occurrence: Occurrence {
                possible: self.occurrence.possible || other.occurrence.possible,
                mandatory: self.occurrence.mandatory && other.occurrence.mandatory,
            },
            bad_character: self.bad_character && other.bad_character,
            finite: self.finite.zip(other.finite).and_then(|(mut a, b)| {
                a.extend(b);
                (a.len() <= MAX_ALTERNATIVES).then_some(a)
            }),
            may_be_absent: self.may_be_absent || other.may_be_absent,
            exact_keys: false,
        }
    }

    fn concat(self, other: Self, query: &Query<'_>) -> Self {
        let length = self.length.add(other.length);
        let alphabet = self.alphabet.union(&other.alphabet);
        let crossing = crossing_occurrences(&self.suffix, &other.prefix, query);
        let first = if self.length.min == 0 {
            self.first.union(&other.first)
        } else {
            self.first.clone()
        };
        let last = if other.length.min == 0 {
            self.last.union(&other.last)
        } else {
            other.last.clone()
        };
        let finite = self
            .finite
            .as_ref()
            .zip(other.finite.as_ref())
            .and_then(|(a, b)| {
                (a.len().saturating_mul(b.len()) <= MAX_ALTERNATIVES).then(|| {
                    a.iter()
                        .flat_map(|a| b.iter().map(move |b| format!("{a}{b}")))
                        .collect()
                })
            });
        Self {
            length,
            alphabet: alphabet.clone(),
            first,
            last,
            prefix: self
                .prefix
                .join(&other.prefix, query.limit, false, length, &alphabet),
            suffix: self
                .suffix
                .join(&other.suffix, query.limit, true, length, &alphabet),
            occurrence: Occurrence {
                possible: self.occurrence.possible
                    || other.occurrence.possible
                    || crossing.possible,
                mandatory: self.occurrence.mandatory
                    || other.occurrence.mandatory
                    || crossing.mandatory,
            },
            bad_character: self.bad_character || other.bad_character,
            finite,
            may_be_absent: self.may_be_absent || other.may_be_absent,
            exact_keys: false,
        }
    }
}

fn token_occurrence(window: &[Characters], token: &str, ignore_ascii_case: bool) -> Occurrence {
    let chars = token.chars().collect::<Vec<_>>();
    if window.len() < chars.len() {
        return Occurrence::default();
    }
    let mut possible = true;
    let mut mandatory = true;
    for (set, ch) in window.iter().zip(chars) {
        let expected = if ignore_ascii_case && ch.is_ascii_alphabetic() {
            Characters::one(ch.to_ascii_lowercase())
                .union(&Characters::one(ch.to_ascii_uppercase()))
        } else {
            Characters::one(ch)
        };
        possible &= !set.disjoint(&expected);
        mandatory &= set.subset(&expected);
    }
    Occurrence {
        possible,
        mandatory,
    }
}

fn crossing_occurrences(a: &Windows, b: &Windows, query: &Query<'_>) -> Occurrence {
    use InputPredicate::*;
    if !matches!(query.predicate, NoSubstring(_) | NoAdjacent { .. }) {
        return Occurrence::default();
    }
    if !query.supported_window {
        return Occurrence {
            possible: true,
            mandatory: false,
        };
    }
    let mut result = Occurrence {
        possible: false,
        mandatory: true,
    };
    for a in &a.0 {
        for b in &b.0 {
            let found = match query.predicate {
                NoSubstring(token) => {
                    let joined = a.iter().chain(b).cloned().collect::<Vec<_>>();
                    let size = token.as_str().chars().count();
                    let mut found = Occurrence::default();
                    for start in 0..a.len() {
                        if start + size <= a.len() || start + size > joined.len() {
                            continue;
                        }
                        let occurrence =
                            token_occurrence(&joined[start..start + size], token.as_str(), false);
                        found.possible |= occurrence.possible;
                        found.mandatory |= occurrence.mandatory;
                    }
                    found
                }
                NoAdjacent { left, right } => {
                    a.last()
                        .zip(b.first())
                        .map_or(Occurrence::default(), |(a, b)| Occurrence {
                            possible: !a.disjoint(&Characters::from_set(left))
                                && !b.disjoint(&Characters::from_set(right)),
                            mandatory: a.subset(&Characters::from_set(left))
                                && b.subset(&Characters::from_set(right)),
                        })
                }
                _ => unreachable!(),
            };
            result.possible |= found.possible;
            result.mandatory &= found.mandatory;
        }
    }
    result
}

struct Estimator<'a> {
    types: Option<&'a TypeFacts>,
    literal_key_type: Option<(
        &'a CompiledAttributeType,
        &'a BTreeMap<String, CompiledAttributeType>,
    )>,
    query: Query<'a>,
    steps: usize,
}

fn path_may_be_absent<'a>(
    mut ty: &'a CompiledAttributeType,
    segments: &[PathSegment],
    defs: &'a BTreeMap<String, CompiledAttributeType>,
) -> bool {
    for step in segments {
        match (step, ty.shape_with_defs(defs)) {
            (PathSegment::Field { name }, crate::schema::Shape::Struct { .. }) => {
                let Some(fields) = crate::schema::struct_fields_with_defs(ty, defs) else {
                    return true;
                };
                if fields
                    .iter()
                    .find(|field| field.name == *name)
                    .is_none_or(|field| field.parts().declared_input_mode == InputMode::Optional)
                {
                    return true;
                }
            }
            // A particular map key or list index need not be present.
            _ => return true,
        }
        let Ok(Some(next)) = narrow_type(
            ty,
            std::slice::from_ref(step),
            defs,
            "",
            InvalidAccessPolicy::Unchecked,
        ) else {
            return true;
        };
        ty = next;
    }
    false
}

impl Estimator<'_> {
    fn spend(&mut self, depth: usize) -> bool {
        if depth >= MAX_DEPTH || self.steps >= MAX_STEPS {
            return false;
        }
        self.steps += 1;
        true
    }

    fn value<P: ValueKind>(&mut self, value: &Value<P>, depth: usize) -> Summary {
        if !self.spend(depth) {
            return Summary::unknown(&self.query);
        }
        match peel_secret(value) {
            Value::Concrete(ConcreteValue::String(value)) => {
                Summary::literal(value.expose_plaintext(), &self.query)
            }
            Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                Summary::literal(value.api_value(), &self.query)
            }
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                self.reference(path, &[], false, depth + 1)
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                let mut summary = Summary::literal("", &self.query);
                for part in parts {
                    let next = match part {
                        InterpolationPart::Literal(value) => {
                            Summary::literal(value.expose_plaintext(), &self.query)
                        }
                        InterpolationPart::Expr(value) => self.value(value, depth + 1),
                    };
                    summary = summary.concat(next, &self.query);
                }
                summary
            }
            // Functions, erased Unknown, non-string conversions and unsupported
            // access forms do not acquire type facts from their result shape.
            _ => Summary::unknown(&self.query),
        }
    }

    fn reference(
        &mut self,
        path: &AccessPath,
        tail: &[PathSegment],
        keys: bool,
        depth: usize,
    ) -> Summary {
        if !self.spend(depth) {
            return Summary::unknown(&self.query);
        }
        let Some(types) = self.types else {
            return Summary::unknown(&self.query);
        };
        let mut binding = path.binding_address();
        for _ in 0..MAX_DEPTH {
            let Some(target) = types.aliases.get(binding) else {
                break;
            };
            binding = target;
        }
        if types.aliases.contains_key(binding) {
            return Summary::unknown(&self.query);
        }
        let segments = path
            .segments()
            .iter()
            .chain(tail)
            .cloned()
            .collect::<Vec<_>>();
        match types.bindings.get(binding) {
            Some(type_context::Declaration::Schema(schema)) => {
                let schema = schema.as_schema();
                let Some(attr) = schema.attributes().get(path.attribute()) else {
                    return Summary::unknown(&self.query);
                };
                let Ok(Some(ty)) = narrow_type(
                    attr.attr_type(),
                    &segments,
                    schema.defs(),
                    "",
                    InvalidAccessPolicy::Unchecked,
                ) else {
                    return Summary::unknown(&self.query);
                };
                let mut summary = self.declared(ty, schema.defs(), keys, depth + 1);
                // Absence is distinct from an empty string. It must never turn
                // a universally failing present type into a failing request.
                summary.may_be_absent |= attr.input_mode() == InputMode::Optional
                    || path_may_be_absent(attr.attr_type(), &segments, schema.defs());
                summary
            }
            Some(type_context::Declaration::Outputs(outputs)) => {
                match outputs.get(path.attribute()) {
                    Some(value) => self.at_path(value, &segments, keys, depth + 1),
                    None => Summary::unknown(&self.query),
                }
            }
            None => Summary::unknown(&self.query),
        }
    }

    fn at_path<P: ValueKind>(
        &mut self,
        value: &Value<P>,
        path: &[PathSegment],
        keys: bool,
        depth: usize,
    ) -> Summary {
        if !self.spend(depth) {
            return Summary::unknown(&self.query);
        }
        let value = peel_secret(value);
        if let Value::Deferred(DeferredValue::ResourceRef { path: reference }) = value {
            return self.reference(reference, path, keys, depth + 1);
        }
        if let Some((step, rest)) = path.split_first() {
            let next = match (value, step) {
                (Value::Concrete(ConcreteValue::Map(map)), PathSegment::Field { name }) => {
                    map.get(name)
                }
                (
                    Value::Concrete(ConcreteValue::Map(map)),
                    PathSegment::Subscript {
                        index: Subscript::Str { key },
                    },
                ) => map.get(key.expose_plaintext()),
                (
                    Value::Concrete(ConcreteValue::List(list)),
                    PathSegment::Subscript {
                        index: Subscript::Int { index },
                    },
                ) => usize::try_from(*index).ok().and_then(|i| list.get(i)),
                _ => None,
            };
            return match next {
                Some(value) => self.at_path(value, rest, keys, depth + 1),
                None => Summary::unknown(&self.query),
            };
        }
        if keys {
            // A map reference need not contain *any* keys. Key types can prove
            // validity, but cannot prove a violation without a nonempty map.
            if let Value::Concrete(ConcreteValue::Map(map)) = value {
                let Some((key_type, defs)) = self.literal_key_type else {
                    return Summary::unknown(&self.query);
                };
                let Ok(keys) = map
                    .keys()
                    .map(|key| {
                        crate::value::canonicalize_authored_map_key_text(key, key_type, defs)
                    })
                    .collect::<Result<Vec<_>, _>>()
                else {
                    return Summary::unknown(&self.query);
                };
                let mut summary = any_of(
                    keys.iter().map(|key| Summary::literal(key, &self.query)),
                    &self.query,
                )
                .unwrap_or_else(|| Summary::literal("", &self.query));
                summary.finite = Some(keys);
                summary.exact_keys = true;
                return summary;
            }
            Summary::unknown(&self.query)
        } else {
            self.value(value, depth + 1)
        }
    }

    fn declared(
        &mut self,
        ty: &CompiledAttributeType,
        defs: &BTreeMap<String, CompiledAttributeType>,
        keys: bool,
        depth: usize,
    ) -> Summary {
        if !self.spend(depth) {
            return Summary::unknown(&self.query);
        }
        match ty.kind() {
            AttrTypeKind::Ref(name) => defs
                .get(name)
                .map(|ty| self.declared(ty, defs, keys, depth + 1))
                .unwrap_or_else(|| Summary::unknown(&self.query)),
            AttrTypeKind::Map {
                key, constraints, ..
            } if keys => {
                // A target enum may rename keys after the reference resolves.
                // Source character facts alone cannot describe that transform.
                let mut summary = self.declared(key, defs, false, depth + 1);
                if self
                    .literal_key_type
                    .is_none_or(|(key, defs)| !key_spelling_is_stable(key, defs, 0))
                {
                    let Some((key_type, target_defs)) = self.literal_key_type else {
                        return Summary::unknown(&self.query);
                    };
                    let Some(values) = &summary.finite else {
                        return Summary::unknown(&self.query);
                    };
                    let Ok(keys) = values
                        .iter()
                        .map(|key| {
                            crate::value::canonicalize_authored_map_key_text(
                                key,
                                key_type,
                                target_defs,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()
                    else {
                        return Summary::unknown(&self.query);
                    };
                    summary = any_of(
                        keys.iter().map(|key| Summary::literal(key, &self.query)),
                        &self.query,
                    )
                    .unwrap_or_else(|| Summary::unknown(&self.query));
                }
                summary.may_be_absent |=
                    LengthInterval::from_bounds(constraints.length().as_ref()).min == 0;
                summary
            }
            _ if keys => Summary::unknown(&self.query),
            AttrTypeKind::String {
                pattern, length, ..
            }
            | AttrTypeKind::Reference {
                pattern, length, ..
            } => {
                let mut length = LengthInterval::from_bounds(length.as_ref());
                let mut alphabet = Characters::all();
                if let Some((set, pattern_length)) = pattern
                    .as_ref()
                    .and_then(|pattern| pattern.whole_string_class())
                {
                    let Some(intersection) = length.intersect(pattern_length) else {
                        return Summary::unknown(&self.query);
                    };
                    length = intersection;
                    alphabet = Characters::from_set(&set);
                }
                Summary::homogeneous(length, alphabet, &self.query)
            }
            AttrTypeKind::Enum {
                values: Some(values),
                ..
            }
            | AttrTypeKind::StringLiteralSet { values } => any_of(
                values
                    .iter()
                    .map(|value| Summary::literal(value, &self.query)),
                &self.query,
            )
            .unwrap_or_else(|| Summary::unknown(&self.query)),
            AttrTypeKind::Enum {
                base, values: None, ..
            } => self.declared(base, defs, false, depth + 1),
            AttrTypeKind::Union(_) => {
                // Each member is interpreted with the same definition owner;
                // raw_shape is used only to relay these declarations.
                let RawShape::Union(members) = ty.raw_shape() else {
                    unreachable!()
                };
                let query = self.query;
                any_of(
                    members
                        .iter()
                        .map(|member| self.declared(member, defs, false, depth + 1)),
                    &query,
                )
                .unwrap_or_else(|| Summary::unknown(&query))
            }
            _ => Summary::unknown(&self.query),
        }
    }
}

/// Every owner enters through its bound, admitted rule and concrete instance.
pub(super) fn estimate(
    applicable: &BoundInputRule<'_, '_>,
    input: rule_input::EstimatorInput<'_>,
) -> InputVerdict {
    let (expression, keys) = input.into_parts();
    let rule = applicable.rule;
    if matches!(
        rule.predicate(),
        InputPredicate::Matches(_) | InputPredicate::NotMatches(_)
    ) {
        return InputVerdict::Undecided;
    }
    let defs = applicable.site.schema.as_schema().defs();
    let mut estimator = Estimator {
        types: Some(expression.declarations()),
        literal_key_type: literal_type(applicable, rule.path()).and_then(|ty| {
            match ty.shape_with_defs(defs) {
                crate::schema::Shape::Map { key, .. } => Some((key, defs)),
                _ => None,
            }
        }),
        query: Query::new(rule.predicate()),
        steps: 0,
    };
    let summary = estimator.at_path(expression.value(), &[], keys, 0);
    decide(summary, rule, &estimator.query)
}

fn key_spelling_is_stable(
    ty: &CompiledAttributeType,
    defs: &BTreeMap<String, CompiledAttributeType>,
    depth: usize,
) -> bool {
    if depth >= MAX_DEPTH {
        return false;
    }
    match ty.kind() {
        AttrTypeKind::String { .. }
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::StringLiteralSet { .. } => true,
        AttrTypeKind::Ref(name) => defs
            .get(name)
            .is_some_and(|ty| key_spelling_is_stable(ty, defs, depth + 1)),
        AttrTypeKind::Union(_) => {
            let RawShape::Union(members) = ty.raw_shape() else {
                unreachable!()
            };
            members
                .iter()
                .all(|ty| key_spelling_is_stable(ty, defs, depth + 1))
        }
        _ => false,
    }
}

fn decide(summary: Summary, rule: &InputRule<Compiled>, query: &Query<'_>) -> InputVerdict {
    use InputPredicate::*;
    let result = if let Some(values) = &summary.finite {
        let passes = values
            .iter()
            .map(|value| evaluate_string(rule, value).is_ok())
            .collect::<Vec<_>>();
        if passes.iter().all(|pass| *pass) {
            Some(true)
        } else if summary.exact_keys || passes.iter().all(|pass| !*pass) {
            Some(false)
        } else {
            None
        }
    } else {
        match rule.predicate() {
            Length { min, max } => {
                let accepted = LengthInterval::from_bounds(Some(&(*min, *max)));
                if accepted.contains(summary.length) {
                    Some(true)
                } else if accepted.disjoint(summary.length) {
                    Some(false)
                } else {
                    None
                }
            }
            AllowedChars(set) => {
                if summary.alphabet.subset(&Characters::from_set(set))
                    || summary.length.max == Some(0)
                {
                    Some(true)
                } else if summary.bad_character {
                    Some(false)
                } else {
                    None
                }
            }
            FirstChar(set) | LastChar(set) => {
                let endpoint = if matches!(rule.predicate(), FirstChar(_)) {
                    &summary.first
                } else {
                    &summary.last
                };
                if summary.length.min > 0 && endpoint.subset(&Characters::from_set(set)) {
                    Some(true)
                } else if endpoint.disjoint(&Characters::from_set(set)) {
                    Some(false)
                } else {
                    None
                }
            }
            NoSubstring(_) | NoAdjacent { .. } => {
                if !summary.occurrence.possible {
                    Some(true)
                } else if summary.occurrence.mandatory {
                    Some(false)
                } else {
                    None
                }
            }
            NoPrefix(token) | NoSuffix(token) => boundary_decision(
                &summary,
                token.as_str(),
                false,
                matches!(rule.predicate(), NoSuffix(_)),
                query,
            ),
            ForbiddenPrefix {
                prefix,
                ignore_ascii_case,
            } => boundary_decision(&summary, prefix.as_str(), *ignore_ascii_case, false, query),
            NotIpv4 => (!summary
                .prefix
                .0
                .iter()
                .any(|window| window_can_be_ipv4(window)))
            .then_some(true),
            Present | Absent | Equals(_) | Matches(_) | NotMatches(_) => None,
        }
    };
    match result {
        Some(true) => InputVerdict::Proven,
        Some(false) if !summary.may_be_absent => InputVerdict::Violation(violation(rule)),
        _ => InputVerdict::Undecided,
    }
}

fn boundary_decision(
    summary: &Summary,
    token: &str,
    fold: bool,
    suffix: bool,
    query: &Query<'_>,
) -> Option<bool> {
    let size = token.chars().count();
    if summary.length.max.is_some_and(|max| max < size as u64) {
        return Some(true);
    }
    if !query.supported_window {
        return None;
    }
    let windows = if suffix {
        &summary.suffix
    } else {
        &summary.prefix
    };
    let occurrences = windows
        .0
        .iter()
        .map(|window| {
            token_occurrence(
                &window[if suffix {
                    window.len().saturating_sub(size)
                } else {
                    0
                }..],
                token,
                fold,
            )
        })
        .collect::<Vec<_>>();
    if occurrences.iter().all(|o| !o.possible) {
        Some(true)
    } else if occurrences.iter().all(|o| o.mandatory) {
        Some(false)
    } else {
        None
    }
}

/// Test whether an over-approximate, complete window could be a strict dotted
/// quad. Valid octets are enumerated as canonical decimal API text, so this
/// shares the concrete predicate's leading-zero and range semantics.
fn window_can_be_ipv4(window: &[Characters]) -> bool {
    if !(7..=15).contains(&window.len()) {
        return false;
    }
    let mut positions = vec![0];
    for octet in 0..4 {
        let mut next = Vec::new();
        for start in positions {
            for value in 0..=255u16 {
                let text = value.to_string();
                let end = start + text.len();
                if end > window.len()
                    || !window[start..end]
                        .iter()
                        .zip(text.chars())
                        .all(|(set, ch)| set.contains(ch))
                {
                    continue;
                }
                if octet == 3 {
                    if end == window.len() {
                        return true;
                    }
                } else if window.get(end).is_some_and(|set| set.contains('.'))
                    && !next.contains(&(end + 1))
                {
                    next.push(end + 1);
                }
            }
        }
        positions = next;
    }
    false
}

fn violation(rule: &InputRule<Compiled>) -> InputViolation {
    rule_violation(rule)
}
