//! Case conversion utilities for Carina DSL to CloudFormation attribute names
//!
//! Carina uses snake_case (e.g., `bucket_name`, `versioning_configuration`)
//! CloudFormation uses CamelCase (e.g., `BucketName`, `VersioningConfiguration`)
//!
//! Enum values are also converted: `enabled` -> `Enabled`

use std::collections::HashMap;

use carina_core::resource::Value;

/// Known enum fields and their valid values
/// Maps field name (CamelCase) to list of valid values (CamelCase)
fn get_enum_values(field_name: &str) -> Option<&'static [&'static str]> {
    match field_name {
        // VersioningConfiguration.Status
        "Status" => Some(&["Enabled", "Suspended"]),
        "AccelerationStatus" => Some(&["Enabled", "Suspended"]),
        _ => None,
    }
}

/// Convert snake_case to CamelCase (PascalCase)
/// e.g., "bucket_name" -> "BucketName"
pub fn to_camel_case(s: &str) -> String {
    s.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

/// Convert CamelCase to snake_case
/// e.g., "BucketName" -> "bucket_name"
pub fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert attributes from snake_case to CamelCase (for sending to AWS)
pub fn attributes_to_camel_case(attributes: &HashMap<String, Value>) -> HashMap<String, Value> {
    attributes
        .iter()
        .map(|(k, v)| {
            let camel_key = to_camel_case(k);
            (camel_key.clone(), value_to_camel_case(v, Some(&camel_key)))
        })
        .collect()
}

/// Convert attributes from CamelCase to snake_case (for reading from AWS)
pub fn attributes_to_snake_case(attributes: &HashMap<String, Value>) -> HashMap<String, Value> {
    attributes
        .iter()
        .map(|(k, v)| (to_snake_case(k), value_to_snake_case(v, Some(k))))
        .collect()
}

/// Recursively convert Value keys to CamelCase
/// Also converts enum string values (e.g., "enabled" -> "Enabled")
fn value_to_camel_case(value: &Value, field_name: Option<&str>) -> Value {
    match value {
        Value::Map(map) => {
            let converted: HashMap<String, Value> = map
                .iter()
                .map(|(k, v)| {
                    let camel_key = to_camel_case(k);
                    (camel_key.clone(), value_to_camel_case(v, Some(&camel_key)))
                })
                .collect();
            Value::Map(converted)
        }
        Value::List(items) => {
            Value::List(items.iter().map(|v| value_to_camel_case(v, None)).collect())
        }
        Value::String(s) => {
            // Check if this field is an enum and convert the value
            if let Some(field) = field_name
                && let Some(valid_values) = get_enum_values(field)
            {
                // Try to match the lowercase value to a valid enum value
                let lower = s.to_lowercase();
                for valid in valid_values {
                    if valid.to_lowercase() == lower {
                        return Value::String((*valid).to_string());
                    }
                }
            }
            Value::String(s.clone())
        }
        other => other.clone(),
    }
}

/// Recursively convert Value keys to snake_case
/// Also converts enum string values to lowercase (e.g., "Enabled" -> "enabled")
fn value_to_snake_case(value: &Value, field_name: Option<&str>) -> Value {
    match value {
        Value::Map(map) => {
            let converted: HashMap<String, Value> = map
                .iter()
                .map(|(k, v)| (to_snake_case(k), value_to_snake_case(v, Some(k))))
                .collect();
            Value::Map(converted)
        }
        Value::List(items) => {
            Value::List(items.iter().map(|v| value_to_snake_case(v, None)).collect())
        }
        Value::String(s) => {
            // Check if this field is an enum and convert to lowercase
            if let Some(field) = field_name
                && get_enum_values(field).is_some()
            {
                return Value::String(s.to_lowercase());
            }
            Value::String(s.clone())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_camel_case() {
        assert_eq!(to_camel_case("bucket_name"), "BucketName");
        assert_eq!(
            to_camel_case("versioning_configuration"),
            "VersioningConfiguration"
        );
        assert_eq!(to_camel_case("status"), "Status");
        assert_eq!(to_camel_case("id"), "Id");
        assert_eq!(to_camel_case("expiration_in_days"), "ExpirationInDays");
    }

    #[test]
    fn test_to_snake_case() {
        assert_eq!(to_snake_case("BucketName"), "bucket_name");
        assert_eq!(
            to_snake_case("VersioningConfiguration"),
            "versioning_configuration"
        );
        assert_eq!(to_snake_case("Status"), "status");
        assert_eq!(to_snake_case("Id"), "id");
        assert_eq!(to_snake_case("ExpirationInDays"), "expiration_in_days");
    }

    #[test]
    fn test_attributes_to_camel_case() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "bucket_name".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert(
            "versioning_configuration".to_string(),
            Value::Map(HashMap::from([(
                "status".to_string(),
                Value::String("enabled".to_string()), // lowercase input
            )])),
        );

        let converted = attributes_to_camel_case(&attrs);

        assert!(converted.contains_key("BucketName"));
        assert!(converted.contains_key("VersioningConfiguration"));

        if let Value::Map(vc) = converted.get("VersioningConfiguration").unwrap() {
            assert!(vc.contains_key("Status"));
            // Enum value should be converted to CamelCase
            assert_eq!(
                vc.get("Status"),
                Some(&Value::String("Enabled".to_string()))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_attributes_to_snake_case() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "BucketName".to_string(),
            Value::String("my-bucket".to_string()),
        );
        attrs.insert(
            "VersioningConfiguration".to_string(),
            Value::Map(HashMap::from([(
                "Status".to_string(),
                Value::String("Enabled".to_string()), // CamelCase input
            )])),
        );

        let converted = attributes_to_snake_case(&attrs);

        assert!(converted.contains_key("bucket_name"));
        assert!(converted.contains_key("versioning_configuration"));

        if let Value::Map(vc) = converted.get("versioning_configuration").unwrap() {
            assert!(vc.contains_key("status"));
            // Enum value should be converted to lowercase
            assert_eq!(
                vc.get("status"),
                Some(&Value::String("enabled".to_string()))
            );
        } else {
            panic!("Expected Map");
        }
    }
}
