//! Type-aware comparison logic for diffing resource attributes.

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
/// - StringEnum: extract enum values from namespaced identifiers and compare
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
                crate::schema::Shape::Float | crate::schema::Shape::Int,
            ) => (*i as f64) == *f && (*i as f64) as i64 == *i,
            (
                Value::Concrete(ConcreteValue::Float(f)),
                Value::Concrete(ConcreteValue::Int(i)),
                crate::schema::Shape::Float | crate::schema::Shape::Int,
            ) => *f == (*i as f64) && (*i as f64) as i64 == *i,

            // Lists: ordered or multiset comparison with inner type awareness
            (
                Value::Concrete(ConcreteValue::List(la)),
                Value::Concrete(ConcreteValue::List(lb)),
                crate::schema::Shape::List { inner, ordered },
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
                crate::schema::Shape::Struct { fields, .. },
            ) => type_aware_struct_equal(ma, mb, fields, defs, secret_ctx),

            // Union: try each member type; if any says equal, they're equal
            (_, _, crate::schema::Shape::Union(types)) => {
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
                            crate::schema::AttrTypeKind::Float | crate::schema::AttrTypeKind::Int
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

            // StringEnum: extract enum values from namespaced identifiers
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
                crate::schema::Shape::StringEnum {
                    values,
                    dsl_aliases,
                    ..
                },
            ) if matches!(
                a,
                Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_))
            ) && matches!(
                b,
                Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_))
            ) =>
            {
                let text = |v: &Value| -> Option<String> {
                    match v {
                        Value::Concrete(ConcreteValue::String(s))
                        | Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.clone()),
                        _ => None,
                    }
                };
                let (Some(sa), Some(sb)) = (text(a), text(b)) else {
                    return a.semantically_equal(b);
                };
                if sa == sb {
                    return true;
                }
                let valid_values: Vec<&str> = values.iter().map(String::as_str).collect();
                let va = crate::utils::extract_enum_value_with_values(&sa, &valid_values);
                let vb = crate::utils::extract_enum_value_with_values(&sb, &valid_values);
                // Map both sides through the DSL alias table to a common
                // (API-canonical) form before comparing. `eq_ignore_ascii_case`
                // alone suffices when the DSL alias differs only in case
                // from the API canonical (`enabled` vs `Enabled`), but it
                // fails on compound-word aliases like
                // `bucket_owner_enforced` vs `BucketOwnerEnforced` — same
                // enum value, different spelling. Aligning both sides to
                // the API spelling closes that gap (aws#271).
                let canonical = |trailing: &str| -> String {
                    // dsl_aliases is `[(api, dsl)]`; if `trailing` matches
                    // the DSL spelling, swap it for the API spelling.
                    for (api, dsl) in dsl_aliases {
                        if dsl.eq_ignore_ascii_case(trailing) {
                            return api.clone();
                        }
                    }
                    trailing.to_string()
                };
                canonical(va).eq_ignore_ascii_case(&canonical(vb))
            }

            // CustomEnum: a namespaced enum carrying a `to_dsl` callback,
            // not an `Aliases` table (StringEnum's shape). The differ sees
            // both shapes in the wild: the provider-read side stores the
            // AWS-canonical value (e.g. `String("ap-northeast-1a")` for
            // `aws.AvailabilityZone`), while the DSL side flows through
            // `resolve_enum_value_recursive` and arrives as the
            // fully-qualified namespaced spelling
            // (`String("aws.AvailabilityZone.ap_northeast_1a")` /
            // `EnumIdentifier(...)` of the same). Without this arm the
            // post-apply plan reports a phantom `forces replacement`
            // diff on every `CustomEnum`-typed attribute — see
            // carina-rs/carina#3312 and the closed carina-rs/carina-provider-aws#363.
            (
                a,
                b,
                crate::schema::Shape::CustomEnum {
                    identity: _,
                    to_dsl,
                    ..
                },
            ) if matches!(
                a,
                Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_))
            ) && matches!(
                b,
                Value::Concrete(ConcreteValue::String(_) | ConcreteValue::EnumIdentifier(_))
            ) =>
            {
                let text = |v: &Value| -> Option<String> {
                    match v {
                        Value::Concrete(ConcreteValue::String(s))
                        | Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.clone()),
                        _ => None,
                    }
                };
                let (Some(sa), Some(sb)) = (text(a), text(b)) else {
                    return a.semantically_equal(b);
                };
                if sa == sb {
                    return true;
                }
                // Normalize both sides to the trailing enum-value spelling,
                // then map through `to_dsl` if available. Each side may
                // arrive in any of these shapes:
                //
                //   - AWS-canonical:     `ap-northeast-1a`
                //   - Dotted namespace:  `aws.AvailabilityZone.ap_northeast_1a`
                //   - With `kind` tail:  `aws.AvailabilityZone.ZoneName.ap_northeast_1a`
                //     (the WIT-bridged identity, where the original
                //     `("aws.AvailabilityZone", "ZoneName")` axis split
                //     does not round-trip and `kind` carries the full
                //     dotted form — see carina#3312)
                //   - Or any of the above with `EnumIdentifier` shape
                //
                // Stripping is greedy: take the segment after the last `.`,
                // since the trailing enum value never contains a `.` in
                // any AWS namespace (`ap_northeast_1a`, `dedicated`, etc.).
                // The `to_dsl` mapping then aligns hyphenated AWS spellings
                // with underscored DSL spellings (`ap-northeast-1a` ↔
                // `ap_northeast_1a`); when `to_dsl` is `None` (the WIT
                // bridge currently drops the function pointer), text
                // equality after stripping is still sufficient for the
                // identical-segment case.
                let canonical = |s: &str| -> String {
                    let trailing = match s.rsplit_once('.') {
                        Some((_, tail)) => tail,
                        None => s,
                    };
                    match to_dsl {
                        Some(f) => f(trailing),
                        None => trailing.to_string(),
                    }
                };
                canonical(&sa) == canonical(&sb)
            }

            // Custom types with base type: delegate to base
            (_, _, crate::schema::Shape::Custom { base, .. }) => {
                type_aware_equal(a, b, Some(base), defs, secret_ctx)
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
/// - String / StringEnum: `""`
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
        (Value::Concrete(ConcreteValue::Int(0)), Some(crate::schema::Shape::Int)) => true,
        (Value::Concrete(ConcreteValue::Float(f)), Some(crate::schema::Shape::Float))
            if *f == 0.0 =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::Duration(d)), Some(crate::schema::Shape::Duration))
            if d.is_zero() =>
        {
            true
        }
        (Value::Concrete(ConcreteValue::String(s)), Some(crate::schema::Shape::String))
            if s.is_empty() =>
        {
            true
        }
        (
            Value::Concrete(ConcreteValue::String(s) | ConcreteValue::EnumIdentifier(s)),
            Some(crate::schema::Shape::StringEnum { .. }),
        ) if s.is_empty() => true,
        (Value::Concrete(ConcreteValue::List(l)), Some(crate::schema::Shape::List { .. }))
            if l.is_empty() =>
        {
            true
        }
        (
            Value::Concrete(ConcreteValue::Map(m)),
            Some(crate::schema::Shape::Map { .. } | crate::schema::Shape::Struct { .. }),
        ) if m.is_empty() => true,
        // Custom types: delegate to the base type
        (_, Some(crate::schema::Shape::Custom { base, .. })) => {
            is_type_default(value, Some(base), defs)
        }
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
        // Skip internal attributes (starting with _)
        if key.starts_with('_') {
            continue;
        }

        // Skip write-only attributes not present in current state.
        // CloudControl API does not return write-only properties, so their
        // absence from state is expected and should not trigger a diff.
        if schema
            .and_then(|s| s.attributes.get(key))
            .is_some_and(|attr| attr.write_only && !projected_current.contains_key(key))
        {
            continue;
        }

        let attr_type = schema
            .and_then(|s| s.attributes.get(key))
            .map(|a| &a.attr_type);

        // Build secret hash context from resource ID and attribute key
        let secret_ctx =
            resource_id.map(|id| SecretHashContext::new(id.display_type(), id.name_str(), key));

        let is_equal = match saved.and_then(|s| s.get(key)) {
            Some(saved_value) => {
                let effective_desired = merge_with_saved(desired_value, saved_value);
                projected_current
                    .get(key)
                    .map(|cv| {
                        type_aware_equal(
                            cv,
                            &effective_desired,
                            attr_type,
                            defs,
                            secret_ctx.as_ref(),
                        )
                    })
                    .unwrap_or(false)
            }
            None => projected_current
                .get(key)
                .map(|cv| type_aware_equal(cv, desired_value, attr_type, defs, secret_ctx.as_ref()))
                .unwrap_or(false),
        };

        if !is_equal {
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
