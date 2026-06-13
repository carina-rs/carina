//! Type-aware comparison logic for diffing resource attributes.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

use crate::explicit::{self, ExplicitFields};
use crate::resource::{ConcreteValue, DeferredValue, ResourceId, Value, merge_with_saved};
use crate::schema::{AttributeType, ResourceSchema, empty_defs_for_schema_walks};
use crate::value::{SECRET_PREFIX, SecretHashContext, argon2id_hash, value_to_json_with_context};

/// Type-aware semantic comparison of two Values.
///
/// When an `AttributeType` is provided, the comparison uses type information
/// to detect semantically equivalent values that differ textually:
/// - Int/Float coercion: `Int(1)` equals `Float(1.0)` for numeric types
/// - List/Map: recurse with inner element type
/// - Struct: recurse with per-field type information, tolerating extra fields
///   with default values (e.g., `false` for Bool)
/// - Enum: extract enum values from namespaced identifiers and compare
///   case-insensitively (e.g., `awscc.s3.Bucket.Type.AES256` equals `"AES256"`)
///
/// Without type information, falls back to `Value::semantically_equal()`.
///
/// When `secret_ctx` is provided, it is used for context-specific salt when
/// comparing `Value::Deferred(DeferredValue::Secret)` against state hash strings.
///
/// # Invariant: `Union[String, list(String)]` is canonicalized upstream
///
/// For attributes typed as `Union[String, list(String)]` (the IAM-style
/// `string_or_list_of_strings` shape — see #2481), **both** `a` and
/// `b` reach this function as the canonical `Value::Concrete(ConcreteValue::StringList)` form,
/// never as a mix of `Value::Concrete(ConcreteValue::String)` and `Value::Concrete(ConcreteValue::List([String]))`. The
/// canonicalization happens upstream in
/// `value::canonicalize_resources_with_schemas` (#2511) for the
/// desired side and `value::canonicalize_states_with_schemas` (#2513)
/// for the actual side, both run before the differ.
///
/// **Do not add a special-case equality here that treats `"x"` and
/// `["x"]` as equal for this Union type.** Doing so would mask the
/// canonicalization invariant: if a non-canonical value reaches the
/// differ, that is an upstream bug and should fail the comparison so
/// the diff surfaces it. Special-case equality at the comparator
/// hides the divergence and leaves state files recording the
/// non-canonical shape, so plan diffs would keep firing on the next
/// run — exactly the phantom-diff regression that #2481 set out to
/// eliminate.
// `pub(crate)` (not `pub(super)`): the plan detail-row renderer
// (`crate::detail_rows`) reuses this exact function so the rendered
// rows agree with `find_changed_attributes`' own verdict. Reusing it
// — never reimplementing — is the carina#3073 fix; a second equality
// notion is precisely the drift this avoids.
pub(crate) fn type_aware_equal(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    secret_ctx: Option<&SecretHashContext>,
) -> bool {
    // Secret comparison: compare the hash of the desired secret with the state hash string.
    // State stores secrets as "_secret:argon2:<hex>", desired has Value::Deferred(DeferredValue::Secret(inner)).
    if let Value::Deferred(DeferredValue::Secret(inner)) = a {
        return secret_matches_state(inner, b, secret_ctx);
    }
    if let Value::Deferred(DeferredValue::Secret(inner)) = b {
        return secret_matches_state(inner, a, secret_ctx);
    }

    // Dispatch through `Shape` so the wildcard `_ => …` arm cannot be
    // reached by a `Ref`-typed attr_type. Shape has no `Ref` variant,
    // so the carina#3340 / carina#3349 invariant is moved from a
    // hand-written `resolve_refs` + `Ref(_) => unreachable!()` guard
    // into the type system.
    let shape = attr_type.map(|t| t.shape_with_defs(defs));

    match shape {
        None => {
            // Even without type info, use type_aware_maps_equal / type_aware_lists_equal
            // for Maps/Lists so that nested Secret values are compared via their hashes.
            // semantically_equal uses PartialEq which doesn't handle Secret↔hash comparison.
            match (a, b) {
                (
                    Value::Concrete(ConcreteValue::Map(ma)),
                    Value::Concrete(ConcreteValue::Map(mb)),
                ) => type_aware_maps_equal(ma, mb, |_key| None, defs, secret_ctx),
                (
                    Value::Concrete(ConcreteValue::List(la)),
                    Value::Concrete(ConcreteValue::List(lb)),
                ) => type_aware_lists_equal(la, lb, None, defs, false, secret_ctx),
                _ => a.semantically_equal(b),
            }
        }
        Some(at) => match (a, b, at) {
            // Int/Float coercion for numeric types
            (
                Value::Concrete(ConcreteValue::Int(i)),
                Value::Concrete(ConcreteValue::Float(f)),
                crate::schema::Shape::Float { .. } | crate::schema::Shape::Int { .. },
            ) => (*i as f64) == *f && (*i as f64) as i64 == *i,
            (
                Value::Concrete(ConcreteValue::Float(f)),
                Value::Concrete(ConcreteValue::Int(i)),
                crate::schema::Shape::Float { .. } | crate::schema::Shape::Int { .. },
            ) => *f == (*i as f64) && (*i as f64) as i64 == *i,

            // String refinements may carry provider/API-to-DSL normalization
            // such as Route53 trailing-dot stripping.
            (
                Value::Concrete(ConcreteValue::String(sa)),
                Value::Concrete(ConcreteValue::String(sb)),
                crate::schema::Shape::String {
                    to_dsl: Some(transform),
                    ..
                },
            ) => transform.apply(sa) == transform.apply(sb),
            (
                Value::Concrete(ConcreteValue::String(sa)),
                Value::Concrete(ConcreteValue::String(sb)),
                crate::schema::Shape::String { to_dsl: None, .. },
            ) => sa == sb,

            // Lists: ordered or multiset comparison with inner type awareness
            (
                Value::Concrete(ConcreteValue::List(la)),
                Value::Concrete(ConcreteValue::List(lb)),
                crate::schema::Shape::List {
                    element_type: inner,
                    ordered,
                    ..
                },
            ) => type_aware_lists_equal(la, lb, Some(inner), defs, ordered, secret_ctx),

            // Maps: recursive comparison with inner value type
            (
                Value::Concrete(ConcreteValue::Map(ma)),
                Value::Concrete(ConcreteValue::Map(mb)),
                crate::schema::Shape::Map { value: inner, .. },
            ) => type_aware_maps_equal(ma, mb, |_key| Some(inner), defs, secret_ctx),

            // Struct: per-field type-aware comparison with default-value tolerance
            (
                Value::Concrete(ConcreteValue::Map(ma)),
                Value::Concrete(ConcreteValue::Map(mb)),
                crate::schema::Shape::Struct { .. },
            ) => {
                let attr_type = attr_type.expect("Some(shape) implies Some(attr_type)");
                let fields = crate::schema::struct_fields_with_defs(attr_type, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                type_aware_struct_equal(ma, mb, fields, defs, secret_ctx)
            }

            // Union: try each member type; if any says equal, they're equal
            (_, _, crate::schema::Shape::Union) => {
                let attr_type = attr_type.expect("Some(shape) implies Some(attr_type)");
                let types = crate::schema::union_members_with_defs(attr_type, defs)
                    .expect("Shape::Union must expose union members internally");
                // Also check Int/Float coercion for unions containing numeric types
                match (a, b) {
                    (
                        Value::Concrete(ConcreteValue::Int(i)),
                        Value::Concrete(ConcreteValue::Float(f)),
                    )
                    | (
                        Value::Concrete(ConcreteValue::Float(f)),
                        Value::Concrete(ConcreteValue::Int(i)),
                    ) if types.iter().any(|t| {
                        matches!(
                            &t.kind,
                            crate::schema::AttrTypeKind::Float { .. }
                                | crate::schema::AttrTypeKind::Int { .. }
                        )
                    }) =>
                    {
                        (*i as f64) == *f && (*i as f64) as i64 == *i
                    }
                    _ => types
                        .iter()
                        .any(|t| type_aware_equal(a, b, Some(t), defs, secret_ctx)),
                }
            }

            // Enum: extract enum values from namespaced identifiers
            // and compare. Phase 5 of carina#2986: accept the cross-shape
            // `String × EnumIdentifier` pair so the differ is stable
            // across "state file stores plain string, desired-side parsed
            // as identifier short form" — the steady-state shape for any
            // enum-typed attribute whose value flowed through a provider
            // `read`. Comparison is text-based and case-insensitive on
            // the enum value, matching the existing String × String arm.
            (
                a,
                b,
                crate::schema::Shape::Enum {
                    values,
                    dsl_aliases,
                    to_dsl,
                    ..
                },
            ) if matches!(
                a,
                Value::Concrete(
                    ConcreteValue::String(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_)
                )
            ) && matches!(
                b,
                Value::Concrete(
                    ConcreteValue::String(_)
                        | ConcreteValue::EnumIdentifier(_)
                        | ConcreteValue::CanonicalEnum(_)
                )
            ) =>
            {
                if let (
                    Value::Concrete(ConcreteValue::CanonicalEnum(left)),
                    Value::Concrete(ConcreteValue::CanonicalEnum(right)),
                ) = (a, b)
                    && left == right
                {
                    return true;
                }
                // `CanonicalEnumValue` equality remains identity-strict:
                // `aws.Region.ap-northeast-1` and
                // `awscc.Region.ap-northeast-1` are different typed values.
                // Diffing answers a narrower question: would applying change
                // provider API text? If strict equality fails, fall through to
                // the canonical text comparison below so cross-provider export
                // flows do not produce phantom updates.
                let text = |v: &Value| -> Option<String> {
                    match v {
                        Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
                        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.to_string()),
                        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
                            Some(c.api_value().to_string())
                        }
                        _ => None,
                    }
                };
                let (Some(sa), Some(sb)) = (text(a), text(b)) else {
                    return a.semantically_equal(b);
                };
                if sa == sb {
                    return true;
                }
                // Temporary compatibility while some provider-read/state
                // paths can still surface unresolved strings. Once every
                // enum-typed state leaf is resolver-lifted, comparison above
                // should be CanonicalEnum × CanonicalEnum.
                let dsl_map = crate::schema::DslMap::new(dsl_aliases, to_dsl);
                let canonical = |s: &str| -> String {
                    let valid_values: Vec<&str> =
                        values.into_iter().flatten().map(String::as_str).collect();
                    let trailing = crate::utils::extract_enum_value_with_values(s, &valid_values);
                    dsl_map.api_for(&dsl_map.dsl_for(trailing))
                };
                canonical(&sa).eq_ignore_ascii_case(&canonical(&sb))
            }

            // `Shape` has no `Ref` variant by construction (see
            // `crate::schema::Shape`'s type-level docs), so the
            // carina#3340 / carina#3349 invariant — "every walk-site
            // peels Ref before matching" — is enforced by the type
            // system rather than by a hand-written `Ref(_) =>
            // unreachable!()` arm.

            // All other cases: fall back to semantic equality
            _ => a.semantically_equal(b),
        },
    }
}

/// List comparison with type-aware element comparison.
/// When `ordered` is true, elements are compared positionally (sequential).
/// When `ordered` is false, elements are compared as multisets (order-insensitive).
fn type_aware_lists_equal(
    a: &[Value],
    b: &[Value],
    inner: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
    ordered: bool,
    secret_ctx: Option<&SecretHashContext>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if ordered {
        // Sequential comparison: element order matters
        a.iter()
            .zip(b.iter())
            .all(|(va, vb)| type_aware_equal(va, vb, inner, defs, secret_ctx))
    } else {
        // Multiset comparison: order-insensitive
        let mut matched = vec![false; b.len()];
        for item_a in a {
            let mut found = false;
            for (j, item_b) in b.iter().enumerate() {
                if !matched[j] && type_aware_equal(item_a, item_b, inner, defs, secret_ctx) {
                    matched[j] = true;
                    found = true;
                    break;
                }
            }
            if !found {
                return false;
            }
        }
        true
    }
}

/// Map comparison with per-key type lookup.
fn type_aware_maps_equal<'a, F>(
    a: &IndexMap<String, Value>,
    b: &IndexMap<String, Value>,
    get_type: F,
    defs: &BTreeMap<String, AttributeType>,
    secret_ctx: Option<&SecretHashContext>,
) -> bool
where
    F: Fn(&str) -> Option<&'a AttributeType>,
{
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, va)| {
        b.get(k)
            .map(|vb| type_aware_equal(va, vb, get_type(k), defs, secret_ctx))
            .unwrap_or(false)
    })
}

/// Struct comparison that tolerates extra fields with default values.
///
/// When comparing structs, one map may have extra keys that the other doesn't.
/// If the extra key's value is the "zero/default" for its type (e.g., `false`
/// for Bool, `0` for Int), the extra field is ignored. This prevents false diffs
/// when AWS returns default values for fields the user didn't specify.
fn type_aware_struct_equal(
    a: &IndexMap<String, Value>,
    b: &IndexMap<String, Value>,
    fields: &[crate::schema::StructField],
    defs: &BTreeMap<String, AttributeType>,
    secret_ctx: Option<&SecretHashContext>,
) -> bool {
    let field_types: HashMap<&str, &AttributeType> = fields
        .iter()
        .map(|f| (f.name.as_str(), &f.field_type))
        .collect();

    // Check all keys present in both maps are equal
    for (k, va) in a {
        match b.get(k) {
            Some(vb) => {
                if !type_aware_equal(
                    va,
                    vb,
                    field_types.get(k.as_str()).copied(),
                    defs,
                    secret_ctx,
                ) {
                    return false;
                }
            }
            None => {
                // Key only in `a` — must be a type default to be tolerated
                let ft = field_types.get(k.as_str()).copied();
                if !is_type_default(va, ft, defs) {
                    return false;
                }
            }
        }
    }

    // Check keys only in `b`
    for (k, vb) in b {
        if a.contains_key(k) {
            continue; // Already checked above
        }
        let ft = field_types.get(k.as_str()).copied();
        if !is_type_default(vb, ft, defs) {
            return false;
        }
    }

    true
}

/// Check if a value is the "zero/default" for its type.
///
/// - Bool: `false`
/// - Int: `0`
/// - Float: `0.0`
/// - String / Enum: `""`
/// - List: empty list
/// - Map / Struct: empty map
/// - Custom: delegates to the base type
fn is_type_default(
    value: &Value,
    attr_type: Option<&AttributeType>,
    defs: &BTreeMap<String, AttributeType>,
) -> bool {
    // Dispatch through `Shape` so the wildcard arm cannot be reached
    // by a `Ref`-typed `attr_type` (carina#3340 / carina#3349). The
    // `Shape` enum has no `Ref` variant by construction, so the type
    // system rather than convention enforces that every default-value
    // classification has seen its target shape.
    let shape = attr_type.map(|t| t.shape_with_defs(defs));
    match (value, shape) {
        (Value::Concrete(ConcreteValue::Bool(false)), Some(crate::schema::Shape::Bool) | None) => {
            true
        }
        (Value::Concrete(ConcreteValue::Int(0)), Some(crate::schema::Shape::Int { .. })) => true,
        (Value::Concrete(ConcreteValue::Float(f)), Some(crate::schema::Shape::Float { .. }))
            if *f == 0.0 =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::Duration(d)), Some(crate::schema::Shape::Duration))
            if d.is_zero() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::String(s)), Some(crate::schema::Shape::String { .. }))
            if s.is_empty() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::String(s)), Some(crate::schema::Shape::Enum { .. }))
            if s.is_empty() =>
        {
            true
        }
        (
            Value::Concrete(ConcreteValue::EnumIdentifier(s)),
            Some(crate::schema::Shape::Enum { .. }),
        ) if s.as_str().is_empty() => true,
        (Value::Concrete(ConcreteValue::List(l)), Some(crate::schema::Shape::List { .. }))
            if l.is_empty() =>
        {
            true
        }
        (
            Value::Concrete(ConcreteValue::Map(m)),
            Some(crate::schema::Shape::Map { .. } | crate::schema::Shape::Struct { .. }),
        ) if m.is_empty() => true,
        _ => false,
    }
}

/// Check if a secret's inner value matches a state value.
///
/// Two cases are handled:
/// 1. **Hash string** (from state file, `--refresh=false`): the state value is
///    `"_secret:argon2:<hex>"`. We hash the inner value and compare hashes.
/// 2. **Plain-text string** (from provider read, `--refresh=true`): the state
///    value is the raw secret. We compare the inner value directly. This is the
///    fix for issue #1249 where secrets nested in Maps (e.g., tags) caused false
///    diffs because the provider returns plain-text values on read.
///
/// When `context` is provided, uses context-specific salt for hashing.
fn secret_matches_state(
    inner: &Value,
    state_value: &Value,
    context: Option<&SecretHashContext>,
) -> bool {
    let Value::Concrete(ConcreteValue::String(state_str)) = state_value else {
        return false;
    };

    // Case 1: State has a hashed value (from state file / --refresh=false)
    if let Some(state_hash) = state_str.strip_prefix(SECRET_PREFIX) {
        let Ok(inner_json) = value_to_json_with_context(inner, context) else {
            return false;
        };
        let Ok(json_str) = serde_json::to_string(&inner_json) else {
            return false;
        };
        let computed_hash = argon2id_hash(json_str.as_bytes(), context);
        return computed_hash == state_hash;
    }

    // Case 2: State has plain-text value (from provider read / --refresh=true).
    // Compare the inner secret value directly against the raw state string.
    match inner {
        Value::Concrete(ConcreteValue::String(inner_str)) => inner_str == state_str,
        _ => false,
    }
}

pub(crate) struct TypedAttr<'a> {
    pub(crate) attr_type: &'a AttributeType,
    pub(crate) defs: &'a BTreeMap<String, AttributeType>,
}

pub(crate) struct AttrComparison<'a> {
    pub(crate) from: Option<&'a Value>,
    pub(crate) to: &'a Value,
    pub(crate) saved: Option<&'a Value>,
    pub(crate) type_info: Option<TypedAttr<'a>>,
    pub(crate) secret_ctx: Option<&'a SecretHashContext>,
}

/// Return whether a single attribute's values differ semantically.
pub(crate) fn should_patch_attr(cmp: AttrComparison<'_>) -> bool {
    let Some(from_value) = cmp.from else {
        return true;
    };
    let (attr_type, defs) = cmp
        .type_info
        .map(|info| (Some(info.attr_type), info.defs))
        .unwrap_or((None, empty_defs_for_schema_walks()));

    match cmp.saved {
        Some(saved_value) => {
            let effective_to = merge_with_saved(cmp.to, saved_value);
            !type_aware_equal(from_value, &effective_to, attr_type, defs, cmp.secret_ctx)
        }
        None => !type_aware_equal(from_value, cmp.to, attr_type, defs, cmp.secret_ctx),
    }
}

/// Return whether a key is eligible for a provider patch and semantically changed.
pub(crate) fn key_should_enter_patch(
    key: &str,
    schema: Option<&ResourceSchema>,
    cmp: AttrComparison<'_>,
) -> bool {
    if key.starts_with('_') {
        return false;
    }
    if schema
        .and_then(|s| s.attributes.get(key))
        .is_some_and(|attr| attr.write_only && cmp.from.is_none())
    {
        return false;
    }
    should_patch_attr(cmp)
}

/// Build a comparison-only view that grafts `Secret` wrappers from source.
///
/// Apply-time resolution unwraps secrets before provider calls. Re-wrapping the
/// comparison view lets the same hash semantics as plan-time diffing run without
/// changing the normalized value sent to the provider patch.
///
/// Matching `(Map, Map)` and equal-length `(List, List)` shapes are walked
/// recursively. Resolved-only map keys are retained as resolved values; source-only
/// keys are ignored unless they contain a secret that cannot be grafted. If a
/// source secret sits behind a divergent shape, there is no unambiguous resolved
/// position to graft it into, so callers get `None` and must fail closed by not
/// patching that key. Callers on normalization paths that change container shape
/// must not assume secret hash comparison remains available.
pub(crate) fn secret_grafted_comparison_view<'a>(
    resolved: &'a Value,
    source: Option<&'a Value>,
) -> Option<Cow<'a, Value>> {
    let Some(source) = source else {
        return Some(Cow::Borrowed(resolved));
    };
    if !contains_secret(source) {
        return Some(Cow::Borrowed(resolved));
    }
    let view = match (resolved, source) {
        (_, Value::Deferred(DeferredValue::Secret(_))) => Cow::Borrowed(source),
        (
            Value::Concrete(ConcreteValue::Map(resolved)),
            Value::Concrete(ConcreteValue::Map(source)),
        ) => {
            let mut out = resolved.clone();
            for (key, resolved_value) in resolved {
                if let Some(source_value) = source.get(key) {
                    out.insert(
                        key.clone(),
                        secret_grafted_comparison_view(resolved_value, Some(source_value))?
                            .into_owned(),
                    );
                }
            }
            if source.iter().any(|(key, source_value)| {
                !resolved.contains_key(key) && contains_secret(source_value)
            }) {
                return None;
            }
            Cow::Owned(Value::Concrete(ConcreteValue::Map(out)))
        }
        (
            Value::Concrete(ConcreteValue::List(resolved)),
            Value::Concrete(ConcreteValue::List(source)),
        ) if resolved.len() == source.len() => Cow::Owned(Value::Concrete(ConcreteValue::List(
            resolved
                .iter()
                .zip(source)
                .map(|(resolved_value, source_value)| {
                    secret_grafted_comparison_view(resolved_value, Some(source_value))
                        .map(Cow::into_owned)
                })
                .collect::<Option<Vec<_>>>()?,
        ))),
        _ => return None,
    };
    Some(view)
}

fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Secret(_)) => true,
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(contains_secret),
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_secret),
        _ => false,
    }
}

/// Find changed attributes between desired and current state.
/// If `saved` is provided, each desired value is merged with the saved value
/// before comparison, filling in unmanaged nested fields.
/// If `prev_explicit` is provided, the actual-state side is **projected**
/// through its tree before comparison so server-side default fields the
/// user never wrote do not surface as diffs (refs awscc#206). The
/// projection is applied per-attribute: each `current[key]` is folded
/// against the matching `prev_explicit` child shape via
/// `crate::explicit::project`. Top-level keys absent from
/// `prev_explicit.children` are treated as the user never having
/// authored them and excluded from the comparison entirely (so their
/// presence in `current` does not surface a removal).
/// `prev_explicit` also drives the explicit-removal detection at the
/// end of this function: the same set of top-level keys is used to
/// flag attributes the user *previously* wrote but no longer mentions.
/// If `schema` is provided, type-aware comparison is used for each attribute.
/// If `resource_id` is provided, it is used to build context-specific salt for
/// secret hash comparison.
pub(super) fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
    saved: Option<&HashMap<String, Value>>,
    prev_explicit: Option<&ExplicitFields>,
    schema: Option<&ResourceSchema>,
    resource_id: Option<&ResourceId>,
) -> Vec<String> {
    let mut changed = Vec::new();

    // Pull the cyclic-struct definition map (`Ref` targets) from the
    // resource schema if available, otherwise an empty map. Walk-sites
    // resolve `Ref` against this map (carina#3340).
    let defs: &BTreeMap<String, AttributeType> = schema
        .map(|s| &s.defs)
        .unwrap_or(empty_defs_for_schema_walks());

    // Project `current` through `prev_explicit` so server-side defaults
    // the user never authored disappear before any comparison runs.
    // The projection is idempotent and shape-preserving for keys the
    // user did author. When `prev_explicit` is absent (e.g. first-plan,
    // no saved state) we fall back to the unprojected map — there is
    // no authoring information to consult yet.
    //
    // `saved` undergoes the same projection so the saved-merge fallback
    // doesn't smuggle server-only fields back into `effective_desired`
    // (refs awscc#206).
    let projected_current = match prev_explicit {
        Some(e) => explicit::project_attributes(current.clone(), e),
        None => current.clone(),
    };
    let projected_saved = match (saved, prev_explicit) {
        (Some(s), Some(e)) => Some(explicit::project_attributes(s.clone(), e)),
        _ => None,
    };
    let saved = projected_saved.as_ref().or(saved);

    for (key, desired_value) in desired {
        let type_info = schema.and_then(|s| {
            s.attributes.get(key).map(|attr| TypedAttr {
                attr_type: &attr.attr_type,
                defs,
            })
        });

        // Build secret hash context from resource ID and attribute key
        let secret_ctx =
            resource_id.map(|id| SecretHashContext::new(id.display_type(), id.name_str(), key));

        if key_should_enter_patch(
            key,
            schema,
            AttrComparison {
                from: projected_current.get(key),
                to: desired_value,
                saved: saved.and_then(|s| s.get(key)),
                type_info,
                secret_ctx: secret_ctx.as_ref(),
            },
        ) {
            changed.push(key.clone());
        }
    }

    // Detect attributes removed from desired but still present in current.
    // Only flag attributes that were previously in the user's desired
    // state (top-level children of `prev_explicit`'s root `Struct`).
    // This prevents false removals for computed/provider-returned
    // attributes the user never specified. With `Unrecorded` (no
    // authoring record) there is nothing to compare against, so
    // attribute removal is not detected for those rows — the
    // `from_provider_state` repair populates `Struct` on the next
    // apply, after which removal detection works normally.
    if let Some(ExplicitFields::Struct { children }) = prev_explicit {
        for key in children.keys() {
            if key.starts_with('_') {
                continue;
            }
            if desired.contains_key(key) {
                continue;
            }
            if projected_current.contains_key(key) {
                changed.push(key.clone());
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DslTransform, TypeIdentity};

    #[test]
    fn refined_string_to_dsl_transform_is_applied_before_comparison() {
        let attr_type = AttributeType::refined_string(
            None,
            None,
            None,
            Some(DslTransform::StripSuffix(".".to_string())),
        );
        let state = Value::Concrete(ConcreteValue::String("foo.example.com.".to_string()));
        let desired = Value::Concrete(ConcreteValue::String("foo.example.com".to_string()));

        assert!(type_aware_equal(
            &state,
            &desired,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
            None,
        ));
    }

    #[test]
    fn canonical_enum_cross_identity_same_api_value_is_no_change() {
        let attr_type = AttributeType::enum_(
            TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
            Some(vec!["ap-northeast-1".to_string()]),
            Vec::new(),
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let producer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));
        let consumer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));

        assert!(type_aware_equal(
            &producer,
            &consumer,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
            None,
        ));
    }

    #[test]
    fn canonical_enum_cross_identity_different_api_value_is_change() {
        let attr_type = AttributeType::enum_(
            TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
            Some(vec!["ap-northeast-1".to_string(), "us-east-1".to_string()]),
            Vec::new(),
            None,
            Some(DslTransform::HyphenToUnderscore),
        );
        let producer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));
        let consumer = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
                "us-east-1",
            ),
        ));

        assert!(!type_aware_equal(
            &producer,
            &consumer,
            Some(&attr_type),
            empty_defs_for_schema_walks(),
            None,
        ));
    }
}
