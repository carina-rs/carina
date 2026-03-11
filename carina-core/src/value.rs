//! Value conversion and formatting utilities

use std::collections::HashMap;

use crate::resource::Value;
use crate::utils::is_dsl_enum_format;

/// Convert `Value` to `serde_json::Value`.
///
/// # Panics
///
/// Panics if `value` contains a non-finite float because JSON cannot represent
/// `NaN` or infinity.
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Int(n) => serde_json::Value::Number((*n).into()),
        Value::Float(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f)
                .unwrap_or_else(|| panic!("cannot convert non-finite float {f} to JSON")),
        ),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<_, _> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } => serde_json::Value::String(format!("${{{}.{}}}", binding_name, attribute_name)),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => serde_json::Value::String(format!("{}.{}", name, m)),
            None => serde_json::Value::String(name.clone()),
        },
    }
}

/// Convert `serde_json::Value` to DSL `Value`
pub fn json_to_dsl_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Array(items) => {
            Value::List(items.iter().map(json_to_dsl_value).collect())
        }
        serde_json::Value::Object(map) => {
            let m: HashMap<_, _> = map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_dsl_value(v)))
                .collect();
            Value::Map(m)
        }
        serde_json::Value::Null => Value::String("null".to_string()),
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
            // DSL enum format (namespaced identifiers) - display without quotes
            if is_dsl_enum_format(s) {
                return s.clone();
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
            let strs: Vec<_> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                .collect();
            format!("{{{}}}", strs.join(", "))
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
            ..
        } => format!("{}.{}", binding_name, attribute_name),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => format!("{}.{}", name, m),
            None => name.clone(),
        },
    }
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
        assert_eq!(value_to_json(&v), serde_json::json!("hello"));
    }

    #[test]
    fn test_value_to_json_int() {
        let v = Value::Int(42);
        assert_eq!(value_to_json(&v), serde_json::json!(42));
    }

    #[test]
    fn test_value_to_json_float() {
        let v = Value::Float(1.5);
        assert_eq!(value_to_json(&v), serde_json::json!(1.5));
    }

    #[test]
    #[should_panic(expected = "cannot convert non-finite float NaN to JSON")]
    fn test_value_to_json_nan_panics() {
        let v = Value::Float(f64::NAN);
        let _ = value_to_json(&v);
    }

    #[test]
    #[should_panic(expected = "cannot convert non-finite float inf to JSON")]
    fn test_value_to_json_infinity_panics() {
        let v = Value::Float(f64::INFINITY);
        let _ = value_to_json(&v);
    }

    #[test]
    fn test_value_to_json_bool() {
        let v = Value::Bool(true);
        assert_eq!(value_to_json(&v), serde_json::json!(true));
    }

    #[test]
    fn test_value_to_json_list() {
        let v = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(value_to_json(&v), serde_json::json!([1, 2]));
    }

    #[test]
    fn test_value_to_json_map() {
        let mut map = HashMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        let v = Value::Map(map);
        assert_eq!(value_to_json(&v), serde_json::json!({"key": "val"}));
    }

    #[test]
    fn test_value_to_json_resource_ref() {
        let v = Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "id".to_string(),
        };
        assert_eq!(value_to_json(&v), serde_json::json!("${vpc.id}"));
    }

    #[test]
    fn test_json_to_dsl_value_string() {
        let j = serde_json::json!("hello");
        assert_eq!(json_to_dsl_value(&j), Value::String("hello".to_string()));
    }

    #[test]
    fn test_json_to_dsl_value_int() {
        let j = serde_json::json!(42);
        assert_eq!(json_to_dsl_value(&j), Value::Int(42));
    }

    #[test]
    fn test_json_to_dsl_value_float() {
        let j = serde_json::json!(1.5);
        assert_eq!(json_to_dsl_value(&j), Value::Float(1.5));
    }

    #[test]
    fn test_json_to_dsl_value_bool() {
        let j = serde_json::json!(true);
        assert_eq!(json_to_dsl_value(&j), Value::Bool(true));
    }

    #[test]
    fn test_json_to_dsl_value_array() {
        let j = serde_json::json!([1, 2]);
        assert_eq!(
            json_to_dsl_value(&j),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn test_json_to_dsl_value_null() {
        let j = serde_json::Value::Null;
        assert_eq!(json_to_dsl_value(&j), Value::String("null".to_string()));
    }

    #[test]
    fn test_roundtrip_value_json() {
        let original = Value::List(vec![
            Value::String("hello".to_string()),
            Value::Int(42),
            Value::Bool(false),
        ]);
        let json = value_to_json(&original);
        let back = json_to_dsl_value(&json);
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
        assert_eq!(format_value(&v), "aws.s3.VersioningStatus.Enabled");
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
        let v = Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "id".to_string(),
        };
        assert_eq!(format_value(&v), "vpc.id");
    }

    #[test]
    fn test_is_list_of_maps_true() {
        let mut map = HashMap::new();
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
        let mut m1 = HashMap::new();
        m1.insert("a".to_string(), Value::Int(1));
        m1.insert("b".to_string(), Value::Int(2));
        let mut m2 = HashMap::new();
        m2.insert("a".to_string(), Value::Int(1));
        m2.insert("b".to_string(), Value::Int(3));
        assert_eq!(map_similarity(&Value::Map(m1), &Value::Map(m2)), 1);
    }

    #[test]
    fn test_map_similarity_non_maps() {
        assert_eq!(map_similarity(&Value::Int(1), &Value::Int(1)), 0);
    }
}
