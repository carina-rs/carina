//! Plan-time normalization of enum identifiers and state hydration.
//!
//! This module contains standalone functions used by `ProviderNormalizer` to resolve
//! enum identifiers in resources and restore unreturned attributes from saved state.

use std::collections::HashMap;

use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::{AttributeType, StructField};

/// Resolve enum identifiers in resources to their fully-qualified DSL format.
///
/// For each awscc resource, looks up the schema and resolves bare identifiers
/// (e.g., `advanced`) or TypeName.value identifiers (e.g., `Tier.advanced`)
/// into fully-qualified namespaced strings (e.g., `awscc.ec2.ipam.Tier.advanced`).
pub fn resolve_enum_identifiers_impl(resources: &mut [Resource]) {
    let awscc_configs = crate::schemas::generated::configs();

    for resource in resources.iter_mut() {
        // Only handle awscc resources
        if resource.id.provider != "awscc" {
            continue;
        }

        // Find the matching schema config
        let config = awscc_configs.iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("awscc.")
                .map(|t| t == resource.id.resource_type)
                .unwrap_or(false)
        });
        let config = match config {
            Some(c) => c,
            None => continue,
        };

        // Resolve enum attributes
        let mut resolved_attrs = HashMap::new();
        for (key, value) in &resource.attributes {
            if let Some(attr_schema) = config.schema.attributes.get(key.as_str())
                && let Some((type_name, ns, to_dsl)) = attr_schema.attr_type.namespaced_enum_parts()
            {
                let resolved = match value {
                    Value::UnresolvedIdent(ident, None) => {
                        // bare identifier: advanced -> awscc.ec2.ipam.Tier.advanced
                        let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                        // TypeName.value: Tier.advanced -> awscc.ec2.ipam.Tier.advanced
                        let dsl_val = to_dsl.map_or_else(|| member.clone(), |f| f(member));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::String(s) if !s.contains('.') => {
                        // plain string: "ap-northeast-1a" -> awscc.AvailabilityZone.ap_northeast_1a
                        let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    _ => value.clone(),
                };
                resolved_attrs.insert(key.clone(), resolved);
                continue;
            }

            // Handle struct fields containing schema-level string enums.
            if let Some(attr_schema) = config.schema.attributes.get(key.as_str()) {
                let struct_fields = match &attr_schema.attr_type {
                    AttributeType::List(inner) => {
                        if let AttributeType::Struct { fields, .. } = inner.as_ref() {
                            Some(fields)
                        } else {
                            None
                        }
                    }
                    AttributeType::Struct { fields, .. } => Some(fields),
                    _ => None,
                };

                if let Some(fields) = struct_fields {
                    let resolved = resolve_struct_enum_values(value, fields);
                    resolved_attrs.insert(key.clone(), resolved);
                    continue;
                }
            }

            resolved_attrs.insert(key.clone(), value.clone());
        }
        resource.attributes = resolved_attrs;
    }
}

/// Resolve enum identifiers within struct field values.
/// Recurses into List and Map values, resolving UnresolvedIdent values
/// for struct fields that have StringEnum type with namespace.
fn resolve_struct_enum_values(value: &Value, fields: &[StructField]) -> Value {
    match value {
        Value::List(items) => {
            let resolved_items: Vec<Value> = items
                .iter()
                .map(|item| resolve_struct_enum_values(item, fields))
                .collect();
            Value::List(resolved_items)
        }
        Value::Map(map) => {
            let mut resolved_map = HashMap::new();
            for (field_key, field_value) in map {
                if let Some(field) = fields.iter().find(|f| f.name == *field_key)
                    && let Some((type_name, ns, to_dsl)) = field.field_type.namespaced_enum_parts()
                {
                    let resolved = match field_value {
                        Value::UnresolvedIdent(ident, None) => {
                            let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                            let dsl_val = to_dsl.map_or_else(|| member.clone(), |f| f(member));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        Value::String(s) if !s.contains('.') => {
                            let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        _ => field_value.clone(),
                    };
                    resolved_map.insert(field_key.clone(), resolved);
                    continue;
                }
                resolved_map.insert(field_key.clone(), field_value.clone());
            }
            Value::Map(resolved_map)
        }
        _ => value.clone(),
    }
}

/// Restore unreturned attributes from saved state into current read states.
///
/// CloudControl API doesn't always return all properties in GetResource responses
/// (create-only properties, and some normal properties like `description`).
/// We carry them forward from the previously saved attribute values.
pub fn restore_unreturned_attrs_impl(
    current_states: &mut HashMap<ResourceId, State>,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
) {
    let awscc_configs = crate::schemas::generated::configs();

    for (resource_id, state) in current_states.iter_mut() {
        if !state.exists || resource_id.provider != "awscc" {
            continue;
        }
        let config = awscc_configs
            .iter()
            .find(|c| c.resource_type_name == resource_id.resource_type);
        let config = match config {
            Some(c) => c,
            None => continue,
        };
        let saved = match saved_attrs.get(resource_id) {
            Some(attrs) => attrs,
            None => continue,
        };
        for dsl_name in config.schema.attributes.keys() {
            if !state.attributes.contains_key(dsl_name)
                && let Some(value) = saved.get(dsl_name)
            {
                state.attributes.insert(dsl_name.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_enum_identifiers_bare_ident() {
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test");
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("dedicated".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["instance_tenancy"] {
            Value::String(s) => assert!(
                s.contains("InstanceTenancy") && s.contains("dedicated"),
                "Expected namespaced enum, got: {}",
                s
            ),
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_typename_value() {
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test");
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string())),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["instance_tenancy"] {
            Value::String(s) => assert!(
                s.contains("InstanceTenancy") && s.contains("dedicated"),
                "Expected namespaced enum, got: {}",
                s
            ),
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_skips_non_awscc() {
        let mut resource = Resource::with_provider("aws", "s3.bucket", "test");
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("dedicated".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert!(matches!(
            &resources[0].attributes["instance_tenancy"],
            Value::UnresolvedIdent(_, _)
        ));
    }

    #[test]
    fn test_resolve_enum_identifiers_hyphen_to_underscore() {
        let mut resource = Resource::with_provider("awscc", "ec2.flow_log", "test");
        resource.attributes.insert(
            "log_destination_type".to_string(),
            Value::UnresolvedIdent("cloud_watch_logs".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["log_destination_type"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.flow_log.LogDestinationType.cloud_watch_logs",
                    "Expected underscored namespaced enum, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_hyphen_string_to_underscore() {
        let mut resource = Resource::with_provider("awscc", "ec2.flow_log", "test");
        resource.attributes.insert(
            "log_destination_type".to_string(),
            Value::String("cloud-watch-logs".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["log_destination_type"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.flow_log.LogDestinationType.cloud_watch_logs",
                    "Hyphenated string should be converted to underscore form, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_restore_unreturned_attrs_impl_create_only() {
        let id = ResourceId::with_provider("awscc", "ec2.nat_gateway", "test");
        let mut state = State::existing(id.clone(), HashMap::new());
        state.attributes.insert(
            "nat_gateway_id".to_string(),
            Value::String("nat-123".to_string()),
        );

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert(
            "subnet_id".to_string(),
            Value::String("subnet-abc".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-abc".to_string()))
        );
    }

    #[test]
    fn test_restore_unreturned_attrs_skips_non_awscc() {
        let id = ResourceId::with_provider("aws", "s3.bucket", "test");
        let state = State::existing(id.clone(), HashMap::new());

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert("some_attr".to_string(), Value::String("value".to_string()));
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        assert!(!current_states[&id].attributes.contains_key("some_attr"));
    }

    #[test]
    fn test_restore_unreturned_attrs_skips_already_present() {
        let id = ResourceId::with_provider("awscc", "ec2.nat_gateway", "test");
        let mut attrs = HashMap::new();
        attrs.insert(
            "subnet_id".to_string(),
            Value::String("subnet-current".to_string()),
        );
        let state = State::existing(id.clone(), attrs);

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert(
            "subnet_id".to_string(),
            Value::String("subnet-saved".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-current".to_string()))
        );
    }

    #[test]
    fn test_restore_unreturned_attrs_impl_non_create_only() {
        let id = ResourceId::with_provider("awscc", "ec2.security_group_egress", "test");
        let mut state = State::existing(id.clone(), HashMap::new());
        state.attributes.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
        );

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert(
            "description".to_string(),
            Value::String("Allow all outbound".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        assert_eq!(
            current_states[&id].attributes.get("description"),
            Some(&Value::String("Allow all outbound".to_string()))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_ip_protocol_all_alias() {
        let mut resource = Resource::with_provider("awscc", "ec2.security_group_egress", "test");
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["ip_protocol"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.security_group_egress.IpProtocol.all",
                    "Expected namespaced IpProtocol.all, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_ip_protocol_tcp() {
        let mut resource = Resource::with_provider("awscc", "ec2.security_group_egress", "test");
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("tcp".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["ip_protocol"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.security_group_egress.IpProtocol.tcp",
                    "Expected namespaced IpProtocol.tcp, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    /// Helper to create struct fields with a Custom enum type for testing
    fn test_ip_protocol_fields() -> Vec<StructField> {
        vec![
            StructField::new(
                "ip_protocol",
                AttributeType::Custom {
                    name: "IpProtocol".to_string(),
                    base: Box::new(AttributeType::String),
                    validate: |_| Ok(()),
                    namespace: Some("awscc.ec2.security_group".to_string()),
                    to_dsl: Some(|s: &str| match s {
                        "-1" => "all".to_string(),
                        _ => s.to_string(),
                    }),
                },
            )
            .with_provider_name("IpProtocol"),
            StructField::new("from_port", AttributeType::Int).with_provider_name("FromPort"),
            StructField::new("cidr_ip", AttributeType::String).with_provider_name("CidrIp"),
        ]
    }

    #[test]
    fn test_resolve_struct_enum_values_bare_ident() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );
        map.insert("from_port".to_string(), Value::Int(443));
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.all");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
                assert_eq!(m["from_port"], Value::Int(443));
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_resolve_struct_enum_values_typename_dot_value() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("IpProtocol".to_string(), Some("tcp".to_string())),
        );
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.tcp");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_resolve_struct_enum_values_string_passthrough() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        map.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group.IpProtocol.tcp".to_string()),
        );
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.tcp");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_impl_struct_field() {
        let mut resource = Resource::with_provider("awscc", "ec2.security_group", "test-sg");
        resource.attributes.insert(
            "group_description".to_string(),
            Value::String("test".to_string()),
        );
        let mut egress_map = HashMap::new();
        egress_map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );
        egress_map.insert(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        resource.attributes.insert(
            "security_group_egress".to_string(),
            Value::List(vec![Value::Map(egress_map)]),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);

        if let Value::List(items) = &resources[0].attributes["security_group_egress"] {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(
                            s, "awscc.ec2.security_group.IpProtocol.all",
                            "Expected namespaced IpProtocol.all in struct field, got: {}",
                            s
                        );
                    }
                    other => panic!("Expected String for ip_protocol, got: {:?}", other),
                }
                match &m["cidr_ip"] {
                    Value::String(s) => assert_eq!(s, "0.0.0.0/0"),
                    other => panic!("Expected String for cidr_ip, got: {:?}", other),
                }
            } else {
                panic!("Expected Map in egress list");
            }
        } else {
            panic!("Expected List for security_group_egress");
        }
    }
}
