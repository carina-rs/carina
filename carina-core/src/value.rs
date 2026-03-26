//! Value conversion and formatting utilities

use std::collections::HashMap;

use crate::resource::{InterpolationPart, Value};
use crate::utils::{convert_enum_value, is_dsl_enum_format};

/// Convert `Value` to `serde_json::Value`.
///
/// Returns an error if `value` contains a non-finite float (NaN or infinity)
/// because JSON cannot represent these values.
pub fn value_to_json(value: &Value) -> Result<serde_json::Value, String> {
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
            let arr: Result<Vec<_>, _> = items.iter().map(value_to_json).collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Map(map) => {
            let obj: Result<serde_json::Map<_, _>, _> = map
                .iter()
                .map(|(k, v)| value_to_json(v).map(|jv| (k.clone(), jv)))
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        Value::ResourceRef {
            binding_name,
            attribute_name,
            field_path,
        } => {
            let mut path = format!("{}.{}", binding_name, attribute_name);
            for field in field_path {
                path.push('.');
                path.push_str(field);
            }
            Ok(serde_json::Value::String(format!("${{{}}}", path)))
        }
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => Ok(serde_json::Value::String(format!("{}.{}", name, m))),
            None => Ok(serde_json::Value::String(name.clone())),
        },
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
            let m: HashMap<_, _> = map
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
        Value::ResourceRef {
            binding_name,
            attribute_name,
            field_path,
        } => {
            let mut path = format!("{}.{}", binding_name, attribute_name);
            for field in field_path {
                path.push('.');
                path.push_str(field);
            }
            path
        }
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => {
                let full = format!("{}.{}", name, m);
                let resolved = convert_enum_value(&full);
                format!("\"{}\"", resolved)
            }
            None => format!("\"{}\"", name),
        },
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
        let mut map = HashMap::new();
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
        let mut map = HashMap::new();
        map.insert("key".to_string(), Value::String("val".to_string()));
        let v = Value::Map(map);
        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!({"key": "val"})
        );
    }

    #[test]
    fn test_value_to_json_resource_ref() {
        let v = Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "id".to_string(),
            field_path: vec![],
        };
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
        let v = Value::String("aws.Region.ap_northeast_1".to_string());
        assert_eq!(format_value(&v), "\"ap-northeast-1\"");
    }

    #[test]
    fn test_format_value_dsl_enum_5_part() {
        let v = Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string());
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_unresolved_ident_with_member() {
        let v =
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string()));
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_unresolved_ident_bare() {
        let v = Value::UnresolvedIdent("dedicated".to_string(), None);
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
        let v = Value::ResourceRef {
            binding_name: "vpc".to_string(),
            attribute_name: "id".to_string(),
            field_path: vec![],
        };
        assert_eq!(format_value(&v), "vpc.id");
    }

    #[test]
    fn test_format_value_resource_ref_with_field_path() {
        let v = Value::ResourceRef {
            binding_name: "web".to_string(),
            attribute_name: "network".to_string(),
            field_path: vec!["vpc_id".to_string()],
        };
        assert_eq!(format_value(&v), "web.network.vpc_id");
    }

    #[test]
    fn test_value_to_json_resource_ref_with_field_path() {
        let v = Value::ResourceRef {
            binding_name: "web".to_string(),
            attribute_name: "output".to_string(),
            field_path: vec!["network".to_string(), "vpc_id".to_string()],
        };
        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!("${web.output.network.vpc_id}")
        );
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
