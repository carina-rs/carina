//! Value conversion between DSL and AWS JSON formats.
//!
//! This module handles bidirectional conversion between Carina's DSL `Value` types
//! and AWS CloudControl API JSON representations. It includes type-aware conversion
//! for enums, structs, lists, maps, and unions.

use std::collections::HashMap;

use carina_core::resource::Value;
use carina_core::schema::AttributeType;
use serde_json::json;

use crate::schemas::generated::canonicalize_enum_value;
use crate::schemas::generated::get_enum_alias_reverse;
use carina_core::utils::{convert_enum_value, extract_enum_value_with_values};

/// Convert AWS value to DSL value
pub(crate) fn aws_value_to_dsl(
    dsl_name: &str,
    value: &serde_json::Value,
    attr_type: &AttributeType,
    resource_type: &str,
) -> Option<Value> {
    // For schema-level string enums with namespace, convert to DSL namespaced format.
    if let Some((type_name, ns, to_dsl)) = attr_type.namespaced_enum_parts()
        && let Some(s) = value.as_str()
    {
        let canonical = if let Some((_, values, _, _)) = attr_type.string_enum_parts() {
            let valid_values: Vec<&str> = values.iter().map(String::as_str).collect();
            canonicalize_enum_value(s, &valid_values)
        } else {
            use crate::schemas::generated::get_enum_valid_values;
            if let Some(valid_values) = get_enum_valid_values(resource_type, dsl_name) {
                canonicalize_enum_value(s, valid_values)
            } else {
                s.to_string()
            }
        };
        // Apply to_dsl transformation if present (e.g., hyphens -> underscores for AZs)
        let dsl_val = to_dsl.map_or_else(|| canonical.clone(), |f| f(&canonical));
        let namespaced = format!("{}.{}.{}", ns, type_name, dsl_val);
        return Some(Value::String(namespaced));
    }

    // For List types, recurse into each item with the inner type for type-aware conversion
    if let AttributeType::List { inner, .. } = attr_type
        && let Some(arr) = value.as_array()
    {
        let items: Vec<Value> = arr
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let result = aws_value_to_dsl(dsl_name, item, inner, resource_type);
                if result.is_none() {
                    log::warn!(
                        "aws_value_to_dsl: dropping unconvertible array item at index {} for attribute '{}' in resource '{}': {:?}",
                        i, dsl_name, resource_type, item
                    );
                }
                result
            })
            .collect();
        return Some(Value::List(items));
    }

    // For Union types, try each member type and use the first that produces a type-aware result
    if let AttributeType::Union(members) = attr_type {
        for member in members {
            if let Some(result) = aws_value_to_dsl(dsl_name, value, member, resource_type) {
                return Some(result);
            }
        }
        return json_to_value(value);
    }

    // For bare Struct{fields}, recurse into fields
    if let AttributeType::Struct { fields, .. } = attr_type
        && let Some(obj) = value.as_object()
    {
        let map: HashMap<String, Value> = fields
            .iter()
            .filter_map(|field| {
                let provider_key = field.provider_name.as_deref().unwrap_or(&field.name);
                let json_val = obj.get(provider_key)?;
                let dsl_val =
                    aws_value_to_dsl(&field.name, json_val, &field.field_type, resource_type);
                dsl_val.map(|v| (field.name.clone(), v))
            })
            .collect();
        if !map.is_empty() {
            return Some(Value::Map(map));
        }
    }

    // For Map types, preserve keys as-is (user-defined) and recurse into values
    if let AttributeType::Map(inner) = attr_type
        && let Some(obj) = value.as_object()
    {
        let map: HashMap<String, Value> = obj
            .iter()
            .filter_map(|(k, v)| {
                let result = aws_value_to_dsl(dsl_name, v, inner, resource_type);
                if result.is_none() {
                    log::warn!(
                        "aws_value_to_dsl: dropping unconvertible map entry '{}' for attribute '{}' in resource '{}': {:?}",
                        k, dsl_name, resource_type, v
                    );
                }
                result.map(|val| (k.clone(), val))
            })
            .collect();
        return Some(Value::Map(map));
    }

    json_to_value(value)
}

/// Convert JSON value to DSL Value
pub(crate) fn json_to_value(value: &serde_json::Value) -> Option<Value> {
    match value {
        serde_json::Value::String(s) => Some(Value::String(s.clone())),
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Int(i))
            } else {
                n.as_f64().map(Value::Float)
            }
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<Value> = arr
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let result = json_to_value(item);
                    if result.is_none() {
                        log::warn!(
                            "json_to_value: dropping unconvertible array item at index {}: {:?}",
                            i,
                            item
                        );
                    }
                    result
                })
                .collect();
            Some(Value::List(items))
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<String, Value> = obj
                .iter()
                .filter_map(|(k, v)| {
                    let result = json_to_value(v);
                    if result.is_none() {
                        log::warn!(
                            "json_to_value: dropping unconvertible map entry '{}': {:?}",
                            k,
                            v
                        );
                    }
                    result.map(|val| (k.clone(), val))
                })
                .collect();
            Some(Value::Map(map))
        }
        _ => None,
    }
}

/// Convert DSL value to AWS JSON value
pub(crate) fn dsl_value_to_aws(
    value: &Value,
    attr_type: &AttributeType,
    resource_type: &str,
    attr_name: &str,
) -> Option<serde_json::Value> {
    // For schema-level string enums, convert namespaced DSL values back to provider values.
    if attr_type.namespaced_enum_parts().is_some() {
        match value {
            Value::String(s) => {
                // Extract the raw enum value from the namespaced identifier, using
                // known valid values for disambiguation when enum values contain dots
                // (e.g., "ipsec.1" in "awscc.ec2.vpn_gateway.Type.ipsec.1").
                let raw = if let Some((_, values, _, _)) = attr_type.string_enum_parts() {
                    let valid: Vec<&str> = values.iter().map(String::as_str).collect();
                    let raw_extracted = extract_enum_value_with_values(s, &valid);
                    canonicalize_enum_value(raw_extracted, &valid)
                } else {
                    convert_enum_value(s)
                };
                // Apply alias reverse mapping (e.g., "all" -> "-1")
                let resolved = match get_enum_alias_reverse(resource_type, attr_name, &raw) {
                    Some(canonical) => canonical.to_string(),
                    None => raw,
                };
                Some(json!(resolved))
            }
            _ => value_to_json(value),
        }
    } else if let AttributeType::List { inner, .. } = attr_type
        && let Value::List(items) = value
    {
        // Recurse into list items with inner type for type-aware conversion
        let arr: Vec<serde_json::Value> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let result = dsl_value_to_aws(item, inner, resource_type, attr_name);
                if result.is_none() {
                    log::warn!(
                        "dsl_value_to_aws: dropping unconvertible list item at index {} for attribute '{}' in resource '{}': {:?}",
                        i, attr_name, resource_type, item
                    );
                }
                result
            })
            .collect();
        Some(serde_json::Value::Array(arr))
    } else if let AttributeType::Union(members) = attr_type {
        // Try each member type; use the first that produces a type-aware result
        for member in members {
            if let Some(result) = dsl_value_to_aws(value, member, resource_type, attr_name) {
                return Some(result);
            }
        }
        value_to_json(value)
    } else if let AttributeType::Struct { fields, .. } = attr_type
        && let Value::Map(map) = value
    {
        // Recurse into bare struct fields for type-aware conversion (map assignment syntax)
        let obj: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .filter_map(|field| {
                let dsl_val = map.get(&field.name)?;
                let provider_key = field
                    .provider_name
                    .as_deref()
                    .unwrap_or(&field.name)
                    .to_string();
                let json_val =
                    dsl_value_to_aws(dsl_val, &field.field_type, resource_type, &field.name);
                json_val.map(|v| (provider_key, v))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else if let AttributeType::Struct { fields, .. } = attr_type
        && let Value::List(items) = value
        && items.len() == 1
        && let Value::Map(map) = &items[0]
    {
        // Recurse into bare struct fields for type-aware conversion (block syntax)
        let obj: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .filter_map(|field| {
                let dsl_val = map.get(&field.name)?;
                let provider_key = field
                    .provider_name
                    .as_deref()
                    .unwrap_or(&field.name)
                    .to_string();
                let json_val =
                    dsl_value_to_aws(dsl_val, &field.field_type, resource_type, &field.name);
                json_val.map(|v| (provider_key, v))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else if let AttributeType::Map(inner) = attr_type
        && let Value::Map(map) = value
    {
        // Map type: preserve keys as-is (user-defined), recurse into values with inner type
        let obj: serde_json::Map<String, serde_json::Value> = map
            .iter()
            .filter_map(|(k, v)| {
                let result = dsl_value_to_aws(v, inner, resource_type, attr_name);
                if result.is_none() {
                    log::warn!(
                        "dsl_value_to_aws: dropping unconvertible map entry '{}' for attribute '{}' in resource '{}': {:?}",
                        k, attr_name, resource_type, v
                    );
                }
                result.map(|val| (k.clone(), val))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else {
        value_to_json(value)
    }
}

/// Convert DSL Value to JSON value
pub(crate) fn value_to_json(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::String(s) => Some(json!(s)),
        Value::Bool(b) => Some(json!(b)),
        Value::Int(i) => Some(json!(i)),
        Value::Float(f) if f.is_finite() => Some(json!(f)),
        Value::Float(_) => None,
        Value::List(items) => {
            let arr: Vec<serde_json::Value> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let result = value_to_json(item);
                    if result.is_none() {
                        log::warn!(
                            "value_to_json: dropping unconvertible list item at index {}: {:?}",
                            i,
                            item
                        );
                    }
                    result
                })
                .collect();
            Some(serde_json::Value::Array(arr))
        }
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter_map(|(k, v)| {
                    let result = value_to_json(v);
                    if result.is_none() {
                        log::warn!(
                            "value_to_json: dropping unconvertible map entry '{}': {:?}",
                            k,
                            v
                        );
                    }
                    result.map(|val| (k.clone(), val))
                })
                .collect();
            Some(serde_json::Value::Object(obj))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::schema::StructField;

    #[test]
    fn test_aws_value_to_dsl_bare_struct_returns_map() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };
        let json_val = serde_json::json!({
            "Status": "Enabled",
        });

        let result = aws_value_to_dsl(
            "versioning_configuration",
            &json_val,
            &attr_type,
            "AWS::S3::Bucket",
        );
        let result = result.expect("Should return Some");

        // Must be Value::Map(...) to match parser output for map assignment syntax
        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("status"),
                Some(&Value::String("Enabled".to_string()))
            );
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_map_for_bare_struct() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Parser produces Value::Map(...) for map assignment syntax (= { ... })
        let mut map = HashMap::new();
        map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        );
        let result = result.expect("Should return Some");

        // Must produce a JSON object (not array)
        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("Status"), Some(&serde_json::json!("Enabled")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_list_for_bare_struct() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Parser produces Value::List(vec![Value::Map(...)]) for block syntax (name { ... })
        let mut map = HashMap::new();
        map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let dsl_value = Value::List(vec![Value::Map(map)]);

        let result = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        );
        let result = result.expect("Should return Some");

        // Must produce a JSON object (not array)
        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("Status"), Some(&serde_json::json!("Enabled")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_bare_struct_roundtrip_no_spurious_diff() {
        let fields =
            vec![StructField::new("status", AttributeType::String).with_provider_name("Status")];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Simulate AWS API response (JSON object)
        let aws_json = serde_json::json!({ "Status": "Enabled" });

        // Read path: convert AWS JSON to DSL value
        let dsl_value = aws_value_to_dsl(
            "versioning_configuration",
            &aws_json,
            &attr_type,
            "AWS::S3::Bucket",
        )
        .expect("read should succeed");

        // Simulate parser output (what the user wrote in .crn with map assignment syntax)
        let mut parser_map = HashMap::new();
        parser_map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let parser_value = Value::Map(parser_map);

        // The read value and parser value must be equal (no spurious diff)
        assert_eq!(
            dsl_value, parser_value,
            "Read value should match parser value — no spurious diff"
        );

        // Write path: convert DSL value back to AWS JSON
        let written_json = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        )
        .expect("write should succeed");

        assert_eq!(
            written_json, aws_json,
            "Round-trip should produce original AWS JSON"
        );
    }

    #[test]
    fn test_vpc_endpoint_type_roundtrip_no_false_diff() {
        let config = crate::schemas::generated::ec2::vpc_endpoint::ec2_vpc_endpoint_config();
        let attr_schema = config.schema.attributes.get("vpc_endpoint_type").unwrap();

        // 1. DSL side: resolve_enum_identifiers_impl converts bare `Gateway` ident
        let mut resource =
            carina_core::resource::Resource::with_provider("awscc", "ec2.vpc_endpoint", "test");
        resource.set_attr("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        resource.set_attr(
            "vpc_endpoint_type".to_string(),
            Value::String("Gateway".to_string()),
        );

        let mut resources = vec![resource];
        crate::provider::resolve_enum_identifiers_impl(&mut resources);

        let dsl_resolved = &resources[0].attributes["vpc_endpoint_type"];
        assert_eq!(
            dsl_resolved,
            &Value::String("awscc.ec2.vpc_endpoint.VpcEndpointType.Gateway".to_string()),
            "DSL bare ident `Gateway` should resolve to namespaced form"
        );

        // 2. AWS read-back side: aws_value_to_dsl converts "Gateway" string
        let aws_json = serde_json::json!("Gateway");
        let aws_dsl = aws_value_to_dsl(
            "vpc_endpoint_type",
            &aws_json,
            &attr_schema.attr_type,
            "ec2.vpc_endpoint",
        )
        .expect("aws_value_to_dsl should return Some");

        assert_eq!(
            aws_dsl,
            Value::String("awscc.ec2.vpc_endpoint.VpcEndpointType.Gateway".to_string()),
            "AWS read-back 'Gateway' should normalize to namespaced form"
        );

        // 3. Both must be equal (no false diff)
        assert_eq!(
            dsl_resolved, &aws_dsl,
            "DSL resolved value and AWS read-back value must match — no false diff"
        );
    }

    #[test]
    fn test_dsl_value_to_aws_preserves_underscores_in_enum_values() {
        let attr_type = AttributeType::StringEnum {
            name: "LogGroupClass".to_string(),
            values: vec![
                "STANDARD".to_string(),
                "INFREQUENT_ACCESS".to_string(),
                "DELIVERY".to_string(),
            ],
            namespace: Some("awscc.logs.log_group".to_string()),
            to_dsl: None,
        };
        let value =
            Value::String("awscc.logs.log_group.LogGroupClass.INFREQUENT_ACCESS".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "logs.log_group", "log_group_class");
        assert_eq!(result, Some(json!("INFREQUENT_ACCESS")));
    }

    #[test]
    fn test_dsl_value_to_aws_converts_underscores_for_region() {
        let attr_type = AttributeType::Custom {
            name: "Region".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: Some("awscc".to_string()),
            to_dsl: None,
        };
        let value = Value::String("awscc.Region.ap_northeast_1".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "logs.log_group", "region");
        assert_eq!(result, Some(json!("ap-northeast-1")));
    }

    #[test]
    fn test_dsl_value_to_aws_list_string_enum() {
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string(), "DELETE".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::list(inner);
        let value = Value::List(vec![
            Value::String("awscc.s3.bucket.AllowedMethod.GET".to_string()),
            Value::String("awscc.s3.bucket.AllowedMethod.PUT".to_string()),
        ]);
        let result = dsl_value_to_aws(&value, &attr_type, "s3.bucket", "allowed_methods");
        assert_eq!(result, Some(json!(["GET", "PUT"])));
    }

    #[test]
    fn test_aws_value_to_dsl_list_string_enum() {
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string(), "DELETE".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::list(inner);
        let json_val = json!(["GET", "PUT"]);
        let result = aws_value_to_dsl("allowed_methods", &json_val, &attr_type, "s3.bucket");
        assert_eq!(
            result,
            Some(Value::List(vec![
                Value::String("awscc.s3.bucket.AllowedMethod.GET".to_string()),
                Value::String("awscc.s3.bucket.AllowedMethod.PUT".to_string()),
            ]))
        );
    }

    #[test]
    fn test_dsl_value_to_aws_list_string_enum_roundtrip() {
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::list(inner);

        let aws_json = json!(["GET", "PUT"]);
        let dsl = aws_value_to_dsl("allowed_methods", &aws_json, &attr_type, "s3.bucket")
            .expect("read should succeed");
        let written = dsl_value_to_aws(&dsl, &attr_type, "s3.bucket", "allowed_methods")
            .expect("write should succeed");
        assert_eq!(written, aws_json, "Round-trip should produce original JSON");
    }

    #[test]
    fn test_dsl_value_to_aws_union_with_string_enum() {
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::String,
        ]);
        let value = Value::String("awscc.ec2.sg.Protocol.tcp".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "ec2.sg", "protocol");
        assert_eq!(result, Some(json!("tcp")));
    }

    #[test]
    fn test_dsl_value_to_aws_map_preserves_user_keys() {
        let attr_type = AttributeType::Map(Box::new(AttributeType::String));

        let mut map = HashMap::new();
        map.insert(
            "my_custom_key".to_string(),
            Value::String("value1".to_string()),
        );
        map.insert(
            "another-key".to_string(),
            Value::String("value2".to_string()),
        );
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(&dsl_value, &attr_type, "s3.bucket", "tags");
        let result = result.expect("Should return Some");

        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("my_custom_key"), Some(&json!("value1")));
            assert_eq!(obj.get("another-key"), Some(&json!("value2")));
            assert!(obj.get("MyCustomKey").is_none());
            assert!(obj.get("AnotherKey").is_none());
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_map_recurses_into_values() {
        let inner_type = AttributeType::StringEnum {
            name: "Status".to_string(),
            values: vec!["Active".to_string(), "Inactive".to_string()],
            namespace: Some("awscc.test.resource".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::Map(Box::new(inner_type));

        let mut map = HashMap::new();
        map.insert(
            "item_one".to_string(),
            Value::String("awscc.test.resource.Status.Active".to_string()),
        );
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(&dsl_value, &attr_type, "test.resource", "status_map");
        let result = result.expect("Should return Some");

        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("item_one"), Some(&json!("Active")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_map_preserves_user_keys() {
        let attr_type = AttributeType::Map(Box::new(AttributeType::String));

        let aws_json = json!({
            "MyCustomKey": "value1",
            "another-key": "value2"
        });

        let result = aws_value_to_dsl("tags", &aws_json, &attr_type, "s3.bucket");
        let result = result.expect("Should return Some");

        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("MyCustomKey"),
                Some(&Value::String("value1".to_string()))
            );
            assert_eq!(
                map.get("another-key"),
                Some(&Value::String("value2".to_string()))
            );
            assert!(map.get("my_custom_key").is_none());
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_union_with_string_enum() {
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::String,
        ]);
        let json_val = json!("tcp");
        let result = aws_value_to_dsl("protocol", &json_val, &attr_type, "ec2.sg");
        assert_eq!(
            result,
            Some(Value::String("awscc.ec2.sg.Protocol.tcp".to_string()))
        );
    }

    #[test]
    fn test_aws_value_to_dsl_union_fallback() {
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::Int,
        ]);
        let json_val = json!(42);
        let result = aws_value_to_dsl("protocol", &json_val, &attr_type, "ec2.sg");
        assert_eq!(result, Some(Value::Int(42)));
    }

    #[test]
    fn test_dsl_value_to_aws_iam_policy_document_uses_pascal_case() {
        use carina_aws_types::iam_policy_document;

        let attr_type = iam_policy_document();
        let value = Value::Map(
            vec![
                (
                    "version".to_string(),
                    Value::String("2012-10-17".to_string()),
                ),
                (
                    "statement".to_string(),
                    Value::List(vec![Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            (
                                "action".to_string(),
                                Value::String("sts:AssumeRole".to_string()),
                            ),
                            (
                                "principal".to_string(),
                                Value::Map(
                                    vec![(
                                        "service".to_string(),
                                        Value::String("lambda.amazonaws.com".to_string()),
                                    )]
                                    .into_iter()
                                    .collect(),
                                ),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let result = dsl_value_to_aws(
            &value,
            &attr_type,
            "iam.role",
            "assume_role_policy_document",
        );
        let result = result.expect("Should return Some");

        let obj = result.as_object().expect("Expected JSON Object");
        assert_eq!(obj.get("Version"), Some(&json!("2012-10-17")));
        assert!(
            obj.get("version").is_none(),
            "snake_case 'version' should not exist"
        );

        let statements = obj.get("Statement").expect("Should have Statement");
        assert!(
            obj.get("statement").is_none(),
            "snake_case 'statement' should not exist"
        );
        let stmt = statements.as_array().unwrap().first().unwrap();
        let stmt_obj = stmt.as_object().unwrap();

        assert_eq!(stmt_obj.get("Effect"), Some(&json!("Allow")));
        assert!(stmt_obj.get("effect").is_none());
        assert_eq!(stmt_obj.get("Action"), Some(&json!("sts:AssumeRole")));
        assert!(stmt_obj.get("action").is_none());

        let principal = stmt_obj.get("Principal").expect("Should have Principal");
        assert!(stmt_obj.get("principal").is_none());
        let principal_obj = principal.as_object().unwrap();
        assert_eq!(
            principal_obj.get("Service"),
            Some(&json!("lambda.amazonaws.com"))
        );
        assert!(principal_obj.get("service").is_none());
    }

    #[test]
    fn test_aws_value_to_dsl_iam_policy_document_uses_snake_case() {
        use carina_aws_types::iam_policy_document;

        let attr_type = iam_policy_document();
        let aws_json = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": "sts:AssumeRole",
                "Principal": {
                    "Service": "lambda.amazonaws.com"
                }
            }]
        });

        let result = aws_value_to_dsl(
            "assume_role_policy_document",
            &aws_json,
            &attr_type,
            "iam.role",
        );
        let result = result.expect("Should return Some");

        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("version"),
                Some(&Value::String("2012-10-17".to_string()))
            );
            assert!(
                map.get("Version").is_none(),
                "PascalCase 'Version' should not exist"
            );

            if let Some(Value::List(stmts)) = map.get("statement") {
                if let Some(Value::Map(stmt)) = stmts.first() {
                    assert_eq!(
                        stmt.get("effect"),
                        Some(&Value::String("Allow".to_string()))
                    );
                    assert_eq!(
                        stmt.get("action"),
                        Some(&Value::String("sts:AssumeRole".to_string()))
                    );
                    if let Some(Value::Map(principal)) = stmt.get("principal") {
                        assert_eq!(
                            principal.get("service"),
                            Some(&Value::String("lambda.amazonaws.com".to_string()))
                        );
                    } else {
                        panic!("Expected principal to be a Map");
                    }
                } else {
                    panic!("Expected statement to contain a Map");
                }
            } else {
                panic!("Expected statement to be a List");
            }
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_region_in_struct_uses_underscores() {
        use crate::schemas::awscc_types::awscc_region;

        let fields = vec![
            StructField::new("region_name", awscc_region())
                .required()
                .with_provider_name("RegionName"),
        ];
        let attr_type = AttributeType::list(AttributeType::Struct {
            name: "IpamOperatingRegion".to_string(),
            fields,
        });
        let json_val = json!([{"RegionName": "ap-northeast-1"}]);

        let result = aws_value_to_dsl("operating_regions", &json_val, &attr_type, "ec2.ipam");
        let expected = Value::List(vec![Value::Map(HashMap::from([(
            "region_name".to_string(),
            Value::String("awscc.Region.ap_northeast_1".to_string()),
        )]))]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_aws_value_to_dsl_enum_value_with_dot() {
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let json_val = json!("ipsec.1");

        let result = aws_value_to_dsl("type", &json_val, &attr_type, "ec2.vpn_gateway");
        assert_eq!(
            result,
            Some(Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }

    #[test]
    fn test_dsl_value_to_aws_enum_value_with_dot() {
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let value = Value::String("awscc.ec2.vpn_gateway.Type.ipsec.1".to_string());

        let result = dsl_value_to_aws(&value, &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(result, Some(json!("ipsec.1")));
    }

    #[test]
    fn test_dsl_value_to_aws_enum_plain_dot_value() {
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let value = Value::String("ipsec.1".to_string());

        let result = dsl_value_to_aws(&value, &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(result, Some(json!("ipsec.1")));
    }

    #[test]
    fn test_enum_round_trip_with_dotted_value() {
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };

        let aws_val = json!("ipsec.1");
        let dsl_val = aws_value_to_dsl("type", &aws_val, &attr_type, "ec2.vpn_gateway");
        assert_eq!(
            dsl_val,
            Some(Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );

        let back_to_aws =
            dsl_value_to_aws(&dsl_val.unwrap(), &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(back_to_aws, Some(json!("ipsec.1")));
    }

    #[test]
    fn test_value_to_json_nan_returns_none() {
        let value = Value::Float(f64::NAN);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_infinity_returns_none() {
        let value = Value::Float(f64::INFINITY);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_neg_infinity_returns_none() {
        let value = Value::Float(f64::NEG_INFINITY);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_finite_float() {
        let value = Value::Float(1.5);
        let result = value_to_json(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), serde_json::json!(1.5));
    }

    #[test]
    fn test_json_to_value_array_with_null_drops_null_items() {
        let json = serde_json::json!(["a", null, "b"]);
        let result = json_to_value(&json);
        let expected = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_json_to_value_map_with_null_value_drops_entry() {
        let json = serde_json::json!({"key1": "val1", "key2": null});
        let result = json_to_value(&json);
        match result {
            Some(Value::Map(map)) => {
                assert_eq!(map.len(), 1);
                assert_eq!(map.get("key1"), Some(&Value::String("val1".to_string())));
                assert!(!map.contains_key("key2"));
            }
            other => panic!("Expected Some(Value::Map), got {:?}", other),
        }
    }

    #[test]
    fn test_aws_value_to_dsl_list_with_null_drops_null_items() {
        let json = serde_json::json!(["a", null, "b"]);
        let attr_type = AttributeType::list(AttributeType::String);
        let result = aws_value_to_dsl("test_attr", &json, &attr_type, "test.resource");
        let expected = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_value_to_json_list_with_nan_drops_nan_items() {
        let value = Value::List(vec![
            Value::Float(1.0),
            Value::Float(f64::NAN),
            Value::Float(2.0),
        ]);
        let result = value_to_json(&value);
        let expected = serde_json::json!([1.0, 2.0]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_dsl_value_to_aws_list_with_nan_drops_nan_items() {
        let value = Value::List(vec![
            Value::Float(1.0),
            Value::Float(f64::NAN),
            Value::Float(2.0),
        ]);
        let attr_type = AttributeType::list(AttributeType::Float);
        let result = dsl_value_to_aws(&value, &attr_type, "test.resource", "test_attr");
        let expected = serde_json::json!([1.0, 2.0]);
        assert_eq!(result, Some(expected));
    }
}
