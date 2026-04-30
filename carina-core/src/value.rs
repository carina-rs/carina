//! Value conversion and formatting utilities

use std::collections::HashMap;

use argon2::Argon2;
use indexmap::IndexMap;

use crate::resource::{InterpolationPart, Value};
use crate::utils::{convert_enum_value, is_dsl_enum_format};

/// Secret value prefix used in state serialization.
pub const SECRET_PREFIX: &str = "_secret:argon2:";

/// Fallback salt for Argon2id hashing when no context is available.
const ARGON2_FALLBACK_SALT: &[u8] = b"carina-secret-v1";

/// Context for deterministic salt generation when hashing secrets.
///
/// The salt is derived from the resource context to ensure that the same
/// password on different resources produces different hashes.
#[derive(Debug, Clone)]
pub struct SecretHashContext {
    pub resource_type: String,
    pub resource_name: String,
    pub attribute_key: String,
}

impl SecretHashContext {
    pub fn new(
        resource_type: impl Into<String>,
        resource_name: impl Into<String>,
        attribute_key: impl Into<String>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            resource_name: resource_name.into(),
            attribute_key: attribute_key.into(),
        }
    }

    /// Build a deterministic salt from the context.
    fn salt(&self) -> String {
        format!(
            "carina:{}:{}:{}",
            self.resource_type, self.resource_name, self.attribute_key
        )
    }
}

/// Hash bytes using Argon2id, returning a hex string.
///
/// When `context` is provided, a deterministic salt derived from the resource
/// context is used. Otherwise, a fixed fallback salt is used.
pub(crate) fn argon2id_hash(input: &[u8], context: Option<&SecretHashContext>) -> String {
    let salt_string;
    let salt: &[u8] = match context {
        Some(ctx) => {
            salt_string = ctx.salt();
            salt_string.as_bytes()
        }
        None => ARGON2_FALLBACK_SALT,
    };
    let mut output = [0u8; 32];
    Argon2::default()
        .hash_password_into(input, salt, &mut output)
        .expect("Argon2id hashing should not fail");
    output.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert `Value` to `serde_json::Value`.
///
/// Returns an error if `value` contains a non-finite float (NaN or infinity)
/// because JSON cannot represent these values.
///
/// For `Value::Secret`, uses the fallback salt. Use `value_to_json_with_context`
/// to provide resource context for deterministic context-specific salt.
pub fn value_to_json(value: &Value) -> Result<serde_json::Value, String> {
    value_to_json_with_context(value, None)
}

/// Convert `Value` to `serde_json::Value` with optional secret hash context.
///
/// When `context` is provided and the value contains `Value::Secret`, the hash
/// uses a deterministic salt derived from the resource context. This ensures
/// that the same password on different resources produces different hashes.
pub fn value_to_json_with_context(
    value: &Value,
    context: Option<&SecretHashContext>,
) -> Result<serde_json::Value, String> {
    match value {
        Value::String(s) => Ok(serde_json::Value::String(s.clone())),
        Value::Int(n) => Ok(serde_json::Value::Number((*n).into())),
        Value::Float(f) => {
            let num = serde_json::Number::from_f64(*f)
                .ok_or_else(|| format!("cannot convert non-finite float {f} to JSON"))?;
            Ok(serde_json::Value::Number(num))
        }
        Value::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        Value::List(items) => {
            let arr: Result<Vec<_>, _> = items
                .iter()
                .map(|item| value_to_json_with_context(item, context))
                .collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Map(map) => {
            let obj: Result<serde_json::Map<_, _>, _> = map
                .iter()
                .map(|(k, v)| value_to_json_with_context(v, context).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        Value::ResourceRef { path } => Ok(serde_json::Value::String(format!(
            "${{{}}}",
            path.to_dot_string()
        ))),
        Value::Interpolation(parts) => {
            let s = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => s.clone(),
                    InterpolationPart::Expr(v) => format_value(v),
                })
                .collect::<String>();
            Ok(serde_json::Value::String(s))
        }
        Value::FunctionCall { name, args } => {
            let arg_strs: Vec<_> = args.iter().map(format_value).collect();
            Ok(serde_json::Value::String(format!(
                "{}({})",
                name,
                arg_strs.join(", ")
            )))
        }
        Value::Secret(inner) => {
            let inner_json = value_to_json_with_context(inner, context)?;
            let json_str = serde_json::to_string(&inner_json)
                .map_err(|e| format!("failed to serialize secret inner value: {e}"))?;
            let hash_hex = argon2id_hash(json_str.as_bytes(), context);
            Ok(serde_json::Value::String(format!(
                "{SECRET_PREFIX}{hash_hex}",
            )))
        }
    }
}

/// Convert `serde_json::Value` to DSL `Value`.
///
/// Returns `None` for JSON null, since null represents a missing/unset value
/// rather than a meaningful attribute value. Callers should filter out `None`
/// entries when building attribute maps.
pub fn json_to_dsl_value(json: &serde_json::Value) -> Option<Value> {
    match json {
        serde_json::Value::String(s) => Some(Value::String(s.clone())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Int(i))
            } else {
                Some(Value::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Array(items) => Some(Value::List(
            items.iter().filter_map(json_to_dsl_value).collect(),
        )),
        serde_json::Value::Object(map) => {
            let m: IndexMap<_, _> = map
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
                .collect();
            Some(Value::Map(m))
        }
        serde_json::Value::Null => None,
    }
}

/// Format a `Value` for display
pub fn format_value(value: &Value) -> String {
    format_value_with_key(value, None)
}

/// Format a `Value` for display, with an optional key for context
pub fn format_value_with_key(value: &Value, _key: Option<&str>) -> String {
    match value {
        Value::String(s) => {
            // Secret hash strings should display as "(secret)" to avoid
            // leaking internal hash representation in plan output
            if s.starts_with(SECRET_PREFIX) {
                return "(secret)".to_string();
            }
            // DSL enum format (namespaced identifiers) - resolve to provider value
            if is_dsl_enum_format(s) {
                let resolved = convert_enum_value(s);
                return format!("\"{}\"", resolved);
            }
            format!("\"{}\"", s)
        }
        Value::Int(n) => n.to_string(),
        Value::Float(f) => {
            let s = f.to_string();
            if s.contains('.') {
                s
            } else {
                format!("{}.0", s)
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::List(items) => {
            let strs: Vec<_> = items.iter().map(format_value).collect();
            format!("[{}]", strs.join(", "))
        }
        Value::Map(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let strs: Vec<_> = keys
                .iter()
                .map(|k| format!("{}: {}", k, format_value(&map[*k])))
                .collect();
            format!("{{{}}}", strs.join(", "))
        }
        Value::ResourceRef { path } => path.to_dot_string(),
        Value::Interpolation(parts) => {
            let inner: String = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => s.clone(),
                    InterpolationPart::Expr(v) => format!("${{{}}}", format_value(v)),
                })
                .collect();
            format!("\"{}\"", inner)
        }
        Value::FunctionCall { name, args } => {
            let arg_strs: Vec<_> = args.iter().map(format_value).collect();
            format!("{}({})", name, arg_strs.join(", "))
        }
        Value::Secret(_) => "(secret)".to_string(),
    }
}

/// Check if a Value contains any Secret values at any nesting depth.
pub fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Secret(_) => true,
        Value::Map(map) => map.values().any(contains_secret),
        Value::List(items) => items.iter().any(contains_secret),
        _ => false,
    }
}

/// Merge secret hashes from the desired value into the provider-returned JSON.
///
/// For attributes containing secrets nested inside Maps or Lists, we cannot simply
/// replace the entire provider value with the desired value's JSON, because the
/// provider may return extra keys (e.g., CloudControl auto-adds tags). This function
/// recursively walks both trees:
/// - If the desired value is `Secret(inner)`, return the hashed value
/// - If desired is a `Map` and provider is an object, merge: for each provider key,
///   if the desired map has a corresponding secret-containing value, use the hashed
///   version; otherwise keep the provider value
/// - If desired is a `List` and provider is an array, merge element-by-element
/// - Otherwise, return the provider value as-is
///
/// When `context` is provided, it is passed through to `value_to_json_with_context`
/// for deterministic context-specific salt in Argon2id hashing.
pub fn merge_secrets_into_provider_json(
    desired: &Value,
    provider_json: &serde_json::Value,
    context: Option<&SecretHashContext>,
) -> Result<serde_json::Value, String> {
    match desired {
        Value::Secret(_) => value_to_json_with_context(desired, context),
        Value::Map(desired_map) => {
            if let serde_json::Value::Object(provider_obj) = provider_json {
                let mut merged = provider_obj.clone();
                for (k, desired_val) in desired_map {
                    if contains_secret(desired_val) {
                        if let Some(provider_val) = provider_obj.get(k) {
                            merged.insert(
                                k.clone(),
                                merge_secrets_into_provider_json(
                                    desired_val,
                                    provider_val,
                                    context,
                                )?,
                            );
                        } else {
                            // Key only in desired (not returned by provider); use desired hash
                            merged.insert(
                                k.clone(),
                                value_to_json_with_context(desired_val, context)?,
                            );
                        }
                    }
                }
                Ok(serde_json::Value::Object(merged))
            } else {
                // Provider didn't return a map; fall back to desired
                value_to_json_with_context(desired, context)
            }
        }
        Value::List(desired_items) => {
            if let serde_json::Value::Array(provider_arr) = provider_json {
                let mut merged = Vec::with_capacity(provider_arr.len());
                for (i, provider_elem) in provider_arr.iter().enumerate() {
                    if let Some(desired_elem) = desired_items.get(i) {
                        if contains_secret(desired_elem) {
                            merged.push(merge_secrets_into_provider_json(
                                desired_elem,
                                provider_elem,
                                context,
                            )?);
                        } else {
                            merged.push(provider_elem.clone());
                        }
                    } else {
                        merged.push(provider_elem.clone());
                    }
                }
                Ok(serde_json::Value::Array(merged))
            } else {
                value_to_json_with_context(desired, context)
            }
        }
        _ => Ok(provider_json.clone()),
    }
}

/// Recursively replace all `Value::Secret(inner)` with `Value::String(hash)`.
///
/// This ensures that when a `Value` tree is serialized (e.g., via serde), no
/// secret plaintext is ever written. The hash uses Argon2id with the fallback
/// salt (not context-aware). This is suitable for plan file serialization where
/// the goal is redaction, not state comparison.
pub fn redact_secrets_in_value(value: &Value) -> Value {
    match value {
        Value::Secret(inner) => {
            let inner_json = value_to_json(inner).unwrap_or(serde_json::Value::Null);
            let json_str = serde_json::to_string(&inner_json).unwrap_or_default();
            let hash_hex = argon2id_hash(json_str.as_bytes(), None);
            Value::String(format!("{SECRET_PREFIX}{hash_hex}"))
        }
        Value::Map(map) => {
            let redacted: IndexMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), redact_secrets_in_value(v)))
                .collect();
            Value::Map(redacted)
        }
        Value::List(items) => Value::List(items.iter().map(redact_secrets_in_value).collect()),
        other => other.clone(),
    }
}

/// Redact all secrets in an attributes map.
pub fn redact_secrets_in_attributes(attrs: &HashMap<String, Value>) -> HashMap<String, Value> {
    attrs
        .iter()
        .map(|(k, v)| (k.clone(), redact_secrets_in_value(v)))
        .collect()
}

/// Redact all secrets in a `Resource`, returning a new Resource with secrets replaced by hashes.
pub fn redact_secrets_in_resource(
    resource: &crate::resource::Resource,
) -> crate::resource::Resource {
    let attributes = resource
        .attributes
        .iter()
        .map(|(k, e)| (k.clone(), redact_secrets_in_value(e)))
        .collect();
    crate::resource::Resource {
        attributes,
        ..resource.clone()
    }
}

/// Redact all secrets in a `State`, returning a new State with secrets replaced by hashes.
pub fn redact_secrets_in_state(state: &crate::resource::State) -> crate::resource::State {
    crate::resource::State {
        id: state.id.clone(),
        identifier: state.identifier.clone(),
        attributes: redact_secrets_in_attributes(&state.attributes),
        exists: state.exists,
        dependency_bindings: state.dependency_bindings.clone(),
    }
}

/// Redact all secrets in an `Effect`, returning a new Effect with secrets replaced by hashes.
pub fn redact_secrets_in_effect(effect: &crate::effect::Effect) -> crate::effect::Effect {
    use crate::effect::Effect;
    match effect {
        Effect::Read { resource } => Effect::Read {
            resource: redact_secrets_in_resource(resource),
        },
        Effect::Create(resource) => Effect::Create(redact_secrets_in_resource(resource)),
        Effect::Update {
            id,
            from,
            to,
            changed_attributes,
        } => Effect::Update {
            id: id.clone(),
            from: Box::new(redact_secrets_in_state(from)),
            to: redact_secrets_in_resource(to),
            changed_attributes: changed_attributes.clone(),
        },
        Effect::Replace {
            id,
            from,
            to,
            lifecycle,
            changed_create_only,
            cascading_updates,
            temporary_name,
            cascade_ref_hints,
        } => Effect::Replace {
            id: id.clone(),
            from: Box::new(redact_secrets_in_state(from)),
            to: redact_secrets_in_resource(to),
            lifecycle: lifecycle.clone(),
            changed_create_only: changed_create_only.clone(),
            temporary_name: temporary_name.clone(),
            cascade_ref_hints: cascade_ref_hints.clone(),
            cascading_updates: cascading_updates
                .iter()
                .map(|cu| crate::effect::CascadingUpdate {
                    id: cu.id.clone(),
                    from: Box::new(redact_secrets_in_state(&cu.from)),
                    to: redact_secrets_in_resource(&cu.to),
                })
                .collect(),
        },
        Effect::Delete {
            id,
            identifier,
            lifecycle,
            binding,
            dependencies,
        } => Effect::Delete {
            id: id.clone(),
            identifier: identifier.clone(),
            lifecycle: lifecycle.clone(),
            binding: binding.clone(),
            dependencies: dependencies.clone(),
        },
        Effect::Import { id, identifier } => Effect::Import {
            id: id.clone(),
            identifier: identifier.clone(),
        },
        Effect::Remove { id } => Effect::Remove { id: id.clone() },
        Effect::Move { from, to } => Effect::Move {
            from: from.clone(),
            to: to.clone(),
        },
    }
}

/// Redact all secrets in a `Plan`, returning a new Plan with secrets replaced by hashes.
pub fn redact_secrets_in_plan(plan: &crate::plan::Plan) -> crate::plan::Plan {
    let mut redacted = crate::plan::Plan::new();
    for effect in plan.effects() {
        redacted.add(redact_secrets_in_effect(effect));
    }
    redacted
}

/// Check if a value is a list of maps (list-of-struct)
pub fn is_list_of_maps(value: &Value) -> bool {
    if let Value::List(items) = value {
        !items.is_empty() && items.iter().all(|item| matches!(item, Value::Map(_)))
    } else {
        false
    }
}

/// Count the number of shared key-value pairs between two map Values.
/// Uses semantically_equal for value comparison so nested lists are order-insensitive.
/// Returns 0 if either value is not a Map.
pub fn map_similarity(a: &Value, b: &Value) -> usize {
    match (a, b) {
        (Value::Map(ma), Value::Map(mb)) => ma
            .iter()
            .filter(|(k, v)| {
                mb.get(*k)
                    .map(|bv| v.semantically_equal(bv))
                    .unwrap_or(false)
            })
            .count(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_to_json_string() {
        let v = Value::String("hello".to_string());
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!("hello"));
    }

    #[test]
    fn test_value_to_json_int() {
        let v = Value::Int(42);
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(42));
    }

    #[test]
    fn test_value_to_json_float() {
        let v = Value::Float(1.5);
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(1.5));
    }

    #[test]
    fn test_value_to_json_nan_returns_error() {
        let v = Value::Float(f64::NAN);
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("NaN"));
    }

    #[test]
    fn test_value_to_json_infinity_returns_error() {
        let v = Value::Float(f64::INFINITY);
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("inf"));
    }

    #[test]
    fn test_value_to_json_neg_infinity_returns_error() {
        let v = Value::Float(f64::NEG_INFINITY);
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("-inf"));
    }

    #[test]
    fn test_value_to_json_nan_in_list_returns_error() {
        let v = Value::List(vec![Value::Int(1), Value::Float(f64::NAN)]);
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("NaN"));
    }

    #[test]
    fn test_value_to_json_nan_in_map_returns_error() {
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::Float(f64::INFINITY));
        let v = Value::Map(map);
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("inf"));
    }

    #[test]
    fn test_value_to_json_bool() {
        let v = Value::Bool(true);
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(true));
    }

    #[test]
    fn test_value_to_json_list() {
        let v = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!([1, 2]));
    }

    #[test]
    fn test_value_to_json_map() {
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        let v = Value::Map(map);
        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!({"key": "val"})
        );
    }

    #[test]
    fn test_value_to_json_resource_ref() {
        let v = Value::resource_ref("vpc", "id", vec![]);
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!("${vpc.id}"));
    }

    #[test]
    fn test_json_to_dsl_value_string() {
        let j = serde_json::json!("hello");
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::String("hello".to_string()))
        );
    }

    #[test]
    fn test_json_to_dsl_value_int() {
        let j = serde_json::json!(42);
        assert_eq!(json_to_dsl_value(&j), Some(Value::Int(42)));
    }

    #[test]
    fn test_json_to_dsl_value_float() {
        let j = serde_json::json!(1.5);
        assert_eq!(json_to_dsl_value(&j), Some(Value::Float(1.5)));
    }

    #[test]
    fn test_json_to_dsl_value_bool() {
        let j = serde_json::json!(true);
        assert_eq!(json_to_dsl_value(&j), Some(Value::Bool(true)));
    }

    #[test]
    fn test_json_to_dsl_value_array() {
        let j = serde_json::json!([1, 2]);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::List(vec![Value::Int(1), Value::Int(2)]))
        );
    }

    #[test]
    fn test_json_to_dsl_value_null() {
        let j = serde_json::Value::Null;
        assert_eq!(json_to_dsl_value(&j), None);
    }

    #[test]
    fn test_json_to_dsl_value_null_in_array() {
        let j = serde_json::json!([1, null, 2]);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::List(vec![Value::Int(1), Value::Int(2)]))
        );
    }

    #[test]
    fn test_json_to_dsl_value_null_in_object() {
        let j = serde_json::json!({"a": 1, "b": null, "c": "hello"});
        let result = json_to_dsl_value(&j).unwrap();
        if let Value::Map(map) = result {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("a"), Some(&Value::Int(1)));
            assert_eq!(map.get("b"), None);
            assert_eq!(map.get("c"), Some(&Value::String("hello".to_string())));
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_roundtrip_value_json() {
        let original = Value::List(vec![
            Value::String("hello".to_string()),
            Value::Int(42),
            Value::Bool(false),
        ]);
        let json = value_to_json(&original).unwrap();
        let back = json_to_dsl_value(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_format_value_string() {
        let v = Value::String("hello".to_string());
        assert_eq!(format_value(&v), "\"hello\"");
    }

    #[test]
    fn test_format_value_dsl_enum() {
        let v = Value::String("aws.s3.VersioningStatus.Enabled".to_string());
        assert_eq!(format_value(&v), "\"Enabled\"");
    }

    #[test]
    fn test_format_value_dsl_enum_region() {
        // Region displays in DSL form (underscored) until provider alias tables
        // are extended to include to_dsl reverse mappings (see issue #1675).
        let v = Value::String("aws.Region.ap_northeast_1".to_string());
        assert_eq!(format_value(&v), "\"ap_northeast_1\"");
    }

    #[test]
    fn test_format_value_dsl_enum_5_part() {
        let v = Value::String("awscc.ec2.Vpc.InstanceTenancy.dedicated".to_string());
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_two_part_enum_string() {
        // Two-part enum strings like "InstanceTenancy.dedicated" are formatted
        // through convert_enum_value which extracts the value part
        let v = Value::String("InstanceTenancy.dedicated".to_string());
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_bare_enum_string() {
        let v = Value::String("dedicated".to_string());
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_int() {
        let v = Value::Int(42);
        assert_eq!(format_value(&v), "42");
    }

    #[test]
    fn test_format_value_float() {
        let v = Value::Float(1.5);
        assert_eq!(format_value(&v), "1.5");
    }

    #[test]
    fn test_format_value_bool() {
        let v = Value::Bool(true);
        assert_eq!(format_value(&v), "true");
    }

    #[test]
    fn test_format_value_list() {
        let v = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(format_value(&v), "[1, 2]");
    }

    #[test]
    fn test_format_value_resource_ref() {
        let v = Value::resource_ref("vpc", "id", vec![]);
        assert_eq!(format_value(&v), "vpc.id");
    }

    #[test]
    fn test_format_value_resource_ref_with_field_path() {
        let v = Value::resource_ref("web", "network", vec!["vpc_id".to_string()]);
        assert_eq!(format_value(&v), "web.network.vpc_id");
    }

    #[test]
    fn test_value_to_json_resource_ref_with_field_path() {
        let v = Value::resource_ref(
            "web",
            "output",
            vec!["network".to_string(), "vpc_id".to_string()],
        );
        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!("${web.output.network.vpc_id}")
        );
    }

    #[test]
    fn test_is_list_of_maps_true() {
        let mut map = IndexMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        let v = Value::List(vec![Value::Map(map)]);
        assert!(is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_empty() {
        let v = Value::List(vec![]);
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_not_maps() {
        let v = Value::List(vec![Value::Int(1)]);
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_not_list() {
        let v = Value::Int(1);
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_map_similarity_matching() {
        let mut m1 = IndexMap::new();
        m1.insert("a".to_string(), Value::Int(1));
        m1.insert("b".to_string(), Value::Int(2));
        let mut m2 = IndexMap::new();
        m2.insert("a".to_string(), Value::Int(1));
        m2.insert("b".to_string(), Value::Int(3));
        assert_eq!(map_similarity(&Value::Map(m1), &Value::Map(m2)), 1);
    }

    #[test]
    fn test_map_similarity_non_maps() {
        assert_eq!(map_similarity(&Value::Int(1), &Value::Int(1)), 0);
    }

    #[test]
    fn test_value_to_json_secret_produces_hash() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let json = value_to_json(&v).unwrap();
        let s = json.as_str().unwrap();
        assert!(
            s.starts_with(SECRET_PREFIX),
            "Expected secret hash prefix, got: {}",
            s
        );
        // Argon2id with 32-byte output = 64 hex characters
        let hash = s.strip_prefix(SECRET_PREFIX).unwrap();
        assert_eq!(hash.len(), 64, "Expected 64-char hex hash, got: {}", hash);
    }

    #[test]
    fn test_value_to_json_secret_is_deterministic() {
        let v1 = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let v2 = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_eq!(json1, json2);
    }

    #[test]
    fn test_value_to_json_secret_different_values_different_hashes() {
        let v1 = Value::Secret(Box::new(Value::String("password-1".to_string())));
        let v2 = Value::Secret(Box::new(Value::String("password-2".to_string())));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_ne!(json1, json2);
    }

    #[test]
    fn test_format_value_secret() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn test_format_value_secret_in_map() {
        let mut map = IndexMap::new();
        map.insert("Name".to_string(), Value::String("test".to_string()));
        map.insert(
            "SecretTag".to_string(),
            Value::Secret(Box::new(Value::String("my-password".to_string()))),
        );
        let v = Value::Map(map);
        let formatted = format_value(&v);
        // Secret values inside maps should show as (secret), not the raw value
        assert!(
            formatted.contains("(secret)"),
            "Expected (secret) in map display, got: {}",
            formatted
        );
        assert!(
            !formatted.contains("my-password"),
            "Should not contain the secret value, got: {}",
            formatted
        );
    }

    #[test]
    fn test_value_to_json_secret_in_map() {
        let mut map = IndexMap::new();
        map.insert("Name".to_string(), Value::String("test".to_string()));
        map.insert(
            "SecretTag".to_string(),
            Value::Secret(Box::new(Value::String("my-password".to_string()))),
        );
        let v = Value::Map(map);
        let json = value_to_json(&v).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("Name").unwrap().as_str().unwrap(), "test");
        let secret_val = obj.get("SecretTag").unwrap().as_str().unwrap();
        assert!(
            secret_val.starts_with(SECRET_PREFIX),
            "Expected secret hash in map value JSON, got: {}",
            secret_val
        );
    }

    #[test]
    fn test_format_value_secret_hash_string() {
        // State stores secret hashes as strings; they should also display as "(secret)"
        let hash_str = format!(
            "{}{}",
            SECRET_PREFIX, "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        let v = Value::String(hash_str);
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn test_value_to_json_with_context_different_resources_different_hashes() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let ctx1 = SecretHashContext::new("ec2.Vpc", "vpc-1", "password");
        let ctx2 = SecretHashContext::new("rds.db_instance", "my-db", "password");
        let json1 = value_to_json_with_context(&v, Some(&ctx1)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx2)).unwrap();
        assert_ne!(
            json1, json2,
            "Same password on different resources should produce different hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_different_attributes_different_hashes() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let ctx1 = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let ctx2 = SecretHashContext::new("rds.db_instance", "my-db", "admin_password");
        let json1 = value_to_json_with_context(&v, Some(&ctx1)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx2)).unwrap();
        assert_ne!(
            json1, json2,
            "Same password on different attributes should produce different hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_same_context_is_deterministic() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let ctx = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let json1 = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        assert_eq!(
            json1, json2,
            "Same password with same context should produce identical hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_differs_from_no_context() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let ctx = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let json_with_ctx = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        let json_no_ctx = value_to_json(&v).unwrap();
        assert_ne!(
            json_with_ctx, json_no_ctx,
            "Context-based hash should differ from fallback hash"
        );
    }

    #[test]
    fn test_redact_secrets_in_value_replaces_secret() {
        let v = Value::Secret(Box::new(Value::String("my-password".to_string())));
        let redacted = redact_secrets_in_value(&v);
        // Should be a String starting with the secret prefix, not a Secret variant
        match &redacted {
            Value::String(s) => {
                assert!(
                    s.starts_with(SECRET_PREFIX),
                    "Expected secret hash prefix, got: {}",
                    s
                );
            }
            _ => panic!(
                "Expected Value::String after redaction, got: {:?}",
                redacted
            ),
        }
    }

    #[test]
    fn test_redact_secrets_in_value_no_plaintext_in_serialized_output() {
        let v = Value::Secret(Box::new(Value::String("super-secret-password".to_string())));
        let redacted = redact_secrets_in_value(&v);
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("super-secret-password"),
            "Serialized output must not contain plaintext secret, got: {}",
            json
        );
    }

    #[test]
    fn test_redact_secrets_in_value_nested_in_map() {
        let mut map = IndexMap::new();
        map.insert("name".to_string(), Value::String("test".to_string()));
        map.insert(
            "password".to_string(),
            Value::Secret(Box::new(Value::String("s3cret".to_string()))),
        );
        let v = Value::Map(map);
        let redacted = redact_secrets_in_value(&v);
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("s3cret"),
            "Serialized map must not contain plaintext secret, got: {}",
            json
        );
        // Non-secret values should be preserved
        assert!(
            json.contains("test"),
            "Non-secret value should be preserved"
        );
    }

    #[test]
    fn test_redact_secrets_in_value_nested_in_list() {
        let v = Value::List(vec![
            Value::String("visible".to_string()),
            Value::Secret(Box::new(Value::String("hidden".to_string()))),
        ]);
        let redacted = redact_secrets_in_value(&v);
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("hidden"),
            "Serialized list must not contain plaintext secret, got: {}",
            json
        );
        assert!(json.contains("visible"));
    }

    #[test]
    fn test_redact_secrets_in_value_preserves_non_secret() {
        let v = Value::String("not-a-secret".to_string());
        let redacted = redact_secrets_in_value(&v);
        assert_eq!(redacted, v);
    }

    #[test]
    fn test_redact_secrets_in_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert(
            "password".to_string(),
            Value::Secret(Box::new(Value::String("hunter2".to_string()))),
        );
        let redacted = redact_secrets_in_attributes(&attrs);
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("hunter2"),
            "Serialized attributes must not contain plaintext secret, got: {}",
            json
        );
        assert!(json.contains("my-bucket"));
    }

    // Closure-shaped tests deleted: `Value::Closure` no longer exists,
    // so `format_value` and `value_to_json` only see user-facing values.
    // The "closure cannot become data" guarantee is now enforced at the
    // type level by `EvalValue::into_value`.
}
