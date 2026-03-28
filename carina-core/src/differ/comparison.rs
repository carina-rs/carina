//! Type-aware comparison logic for diffing resource attributes.

use std::collections::HashMap;

use crate::resource::{ResourceId, Value, merge_with_saved};
use crate::schema::{AttributeType, ResourceSchema};
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
///   case-insensitively (e.g., `awscc.s3.bucket.Type.AES256` equals `"AES256"`)
///
/// Without type information, falls back to `Value::semantically_equal()`.
///
/// When `secret_ctx` is provided, it is used for context-specific salt when
/// comparing `Value::Secret` against state hash strings.
pub(super) fn type_aware_equal(
    a: &Value,
    b: &Value,
    attr_type: Option<&AttributeType>,
    secret_ctx: Option<&SecretHashContext>,
) -> bool {
    // Secret comparison: compare the hash of the desired secret with the state hash string.
    // State stores secrets as "_secret:argon2:<hex>", desired has Value::Secret(inner).
    if let Value::Secret(inner) = a {
        return secret_matches_state(inner, b, secret_ctx);
    }
    if let Value::Secret(inner) = b {
        return secret_matches_state(inner, a, secret_ctx);
    }

    match attr_type {
        None => {
            // Even without type info, use type_aware_maps_equal / type_aware_lists_equal
            // for Maps/Lists so that nested Secret values are compared via their hashes.
            // semantically_equal uses PartialEq which doesn't handle Secret↔hash comparison.
            match (a, b) {
                (Value::Map(ma), Value::Map(mb)) => {
                    type_aware_maps_equal(ma, mb, |_key| None, secret_ctx)
                }
                (Value::List(la), Value::List(lb)) => {
                    type_aware_lists_equal(la, lb, None, false, secret_ctx)
                }
                _ => a.semantically_equal(b),
            }
        }
        Some(at) => match (a, b, at) {
            // Int/Float coercion for numeric types
            (Value::Int(i), Value::Float(f), AttributeType::Float | AttributeType::Int) => {
                (*i as f64) == *f && (*i as f64) as i64 == *i
            }
            (Value::Float(f), Value::Int(i), AttributeType::Float | AttributeType::Int) => {
                *f == (*i as f64) && (*i as f64) as i64 == *i
            }

            // Lists: ordered or multiset comparison with inner type awareness
            (Value::List(la), Value::List(lb), AttributeType::List { inner, ordered }) => {
                type_aware_lists_equal(la, lb, Some(inner), *ordered, secret_ctx)
            }

            // Maps: recursive comparison with inner value type
            (Value::Map(ma), Value::Map(mb), AttributeType::Map(inner)) => {
                type_aware_maps_equal(ma, mb, |_key| Some(inner.as_ref()), secret_ctx)
            }

            // Struct: per-field type-aware comparison with default-value tolerance
            (Value::Map(ma), Value::Map(mb), AttributeType::Struct { fields, .. }) => {
                type_aware_struct_equal(ma, mb, fields, secret_ctx)
            }

            // Union: try each member type; if any says equal, they're equal
            (_, _, AttributeType::Union(types)) => {
                // Also check Int/Float coercion for unions containing numeric types
                match (a, b) {
                    (Value::Int(i), Value::Float(f)) | (Value::Float(f), Value::Int(i))
                        if types
                            .iter()
                            .any(|t| matches!(t, AttributeType::Float | AttributeType::Int)) =>
                    {
                        (*i as f64) == *f && (*i as f64) as i64 == *i
                    }
                    _ => types
                        .iter()
                        .any(|t| type_aware_equal(a, b, Some(t), secret_ctx)),
                }
            }

            // StringEnum: extract enum values from namespaced identifiers and compare
            (Value::String(sa), Value::String(sb), AttributeType::StringEnum { values, .. })
                if sa != sb =>
            {
                let valid_values: Vec<&str> = values.iter().map(String::as_str).collect();
                let va = crate::utils::extract_enum_value_with_values(sa, &valid_values);
                let vb = crate::utils::extract_enum_value_with_values(sb, &valid_values);
                va.eq_ignore_ascii_case(vb)
            }

            // Custom types with base type: delegate to base
            (_, _, AttributeType::Custom { base, .. }) => {
                type_aware_equal(a, b, Some(base), secret_ctx)
            }

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
            .all(|(va, vb)| type_aware_equal(va, vb, inner, secret_ctx))
    } else {
        // Multiset comparison: order-insensitive
        let mut matched = vec![false; b.len()];
        for item_a in a {
            let mut found = false;
            for (j, item_b) in b.iter().enumerate() {
                if !matched[j] && type_aware_equal(item_a, item_b, inner, secret_ctx) {
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
    a: &HashMap<String, Value>,
    b: &HashMap<String, Value>,
    get_type: F,
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
            .map(|vb| type_aware_equal(va, vb, get_type(k), secret_ctx))
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
    a: &HashMap<String, Value>,
    b: &HashMap<String, Value>,
    fields: &[crate::schema::StructField],
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
                if !type_aware_equal(va, vb, field_types.get(k.as_str()).copied(), secret_ctx) {
                    return false;
                }
            }
            None => {
                // Key only in `a` — must be a type default to be tolerated
                let ft = field_types.get(k.as_str()).copied();
                if !is_type_default(va, ft) {
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
        if !is_type_default(vb, ft) {
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
fn is_type_default(value: &Value, attr_type: Option<&AttributeType>) -> bool {
    match (value, attr_type) {
        (Value::Bool(false), Some(AttributeType::Bool) | None) => true,
        (Value::Int(0), Some(AttributeType::Int)) => true,
        (Value::Float(f), Some(AttributeType::Float)) if *f == 0.0 => true,
        (Value::String(s), Some(AttributeType::String)) if s.is_empty() => true,
        (Value::String(s), Some(AttributeType::StringEnum { .. })) if s.is_empty() => true,
        (Value::List(l), Some(AttributeType::List { .. })) if l.is_empty() => true,
        (Value::Map(m), Some(AttributeType::Map(_) | AttributeType::Struct { .. }))
            if m.is_empty() =>
        {
            true
        }
        // Custom types: delegate to the base type
        (_, Some(AttributeType::Custom { base, .. })) => is_type_default(value, Some(base)),
        _ => false,
    }
}

/// Check if a secret's inner value matches a state hash string.
///
/// Hashes the inner value the same way `value_to_json` does for `Value::Secret`,
/// then compares the resulting hash string with the state value.
/// When `context` is provided, uses context-specific salt for hashing.
fn secret_matches_state(
    inner: &Value,
    state_value: &Value,
    context: Option<&SecretHashContext>,
) -> bool {
    let Value::String(state_str) = state_value else {
        return false;
    };
    let Some(state_hash) = state_str.strip_prefix(SECRET_PREFIX) else {
        return false;
    };
    // Compute the hash of the inner value
    let Ok(inner_json) = value_to_json_with_context(inner, context) else {
        return false;
    };
    let Ok(json_str) = serde_json::to_string(&inner_json) else {
        return false;
    };
    let computed_hash = argon2id_hash(json_str.as_bytes(), context);
    computed_hash == state_hash
}

/// Find changed attributes between desired and current state.
/// If `saved` is provided, each desired value is merged with the saved value
/// before comparison, filling in unmanaged nested fields.
/// If `prev_desired_keys` is provided, attributes that were previously in the user's
/// desired state but are now absent from desired (while still present in current)
/// are detected as removals.
/// If `schema` is provided, type-aware comparison is used for each attribute.
/// If `resource_id` is provided, it is used to build context-specific salt for
/// secret hash comparison.
pub(super) fn find_changed_attributes(
    desired: &HashMap<String, Value>,
    current: &HashMap<String, Value>,
    saved: Option<&HashMap<String, Value>>,
    prev_desired_keys: Option<&[String]>,
    schema: Option<&ResourceSchema>,
    resource_id: Option<&ResourceId>,
) -> Vec<String> {
    let mut changed = Vec::new();

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
            .is_some_and(|attr| attr.write_only && !current.contains_key(key))
        {
            continue;
        }

        let attr_type = schema
            .and_then(|s| s.attributes.get(key))
            .map(|a| &a.attr_type);

        // Build secret hash context from resource ID and attribute key
        let secret_ctx =
            resource_id.map(|id| SecretHashContext::new(id.display_type(), &id.name, key));

        let is_equal = match saved.and_then(|s| s.get(key)) {
            Some(saved_value) => {
                let effective_desired = merge_with_saved(desired_value, saved_value);
                current
                    .get(key)
                    .map(|cv| {
                        type_aware_equal(cv, &effective_desired, attr_type, secret_ctx.as_ref())
                    })
                    .unwrap_or(false)
            }
            None => current
                .get(key)
                .map(|cv| type_aware_equal(cv, desired_value, attr_type, secret_ctx.as_ref()))
                .unwrap_or(false),
        };

        if !is_equal {
            changed.push(key.clone());
        }
    }

    // Detect attributes removed from desired but still present in current.
    // Only flag attributes that were previously in the user's desired state
    // (from the state file's desired_keys). This prevents false removals for
    // computed/provider-returned attributes the user never specified.
    if let Some(prev_keys) = prev_desired_keys {
        for key in prev_keys {
            if key.starts_with('_') {
                continue;
            }
            if desired.contains_key(key) {
                continue;
            }
            if current.contains_key(key) {
                changed.push(key.clone());
            }
        }
    }

    changed
}
