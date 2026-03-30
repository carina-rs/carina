//! AWS Provider normalizer and enum resolution

use std::collections::HashMap;

use carina_core::provider::{self, ProviderNormalizer};
use carina_core::resource::{Resource, Value};
use carina_core::schema::ResourceSchema;

/// Schema extension for the AWS provider.
///
/// Handles plan-time normalization of enum identifiers.
pub struct AwsNormalizer;

impl ProviderNormalizer for AwsNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        resolve_enum_identifiers(resources);
    }

    fn merge_default_tags(
        &self,
        resources: &mut [Resource],
        default_tags: &HashMap<String, Value>,
        schemas: &HashMap<String, ResourceSchema>,
    ) {
        provider::merge_default_tags_for_provider("aws", resources, default_tags, schemas);
    }
}

/// Resolve enum identifiers in resources to their fully-qualified DSL format.
///
/// For example, resolves bare `Enabled` or `VersioningStatus.Enabled` into
/// `aws.s3.bucket.VersioningStatus.Enabled` based on schema definitions.
pub(crate) fn resolve_enum_identifiers(resources: &mut [Resource]) {
    let configs = crate::schemas::generated::configs();

    for resource in resources.iter_mut() {
        // Only handle aws resources
        if resource.id.provider != "aws" {
            continue;
        }

        // Find the matching schema config
        let config = configs.iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("aws.")
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
                && let Some(parts) = attr_schema.attr_type.namespaced_enum_parts()
                && let Some(resolved) = carina_core::utils::resolve_enum_value(value, &parts)
            {
                resolved_attrs.insert(key.clone(), resolved);
            }
        }

        for (key, value) in resolved_attrs {
            resource.set_attr(key, value);
        }
    }
}

/// Normalize enum values in read-returned state attributes to namespaced DSL format.
///
/// Read methods return plain values like `"Enabled"` from AWS APIs.
/// This converts them to namespaced format like `aws.s3.bucket.VersioningStatus.Enabled`
/// to match the resolved DSL values.
pub(crate) fn normalize_state_enums(resource_type: &str, attributes: &mut HashMap<String, Value>) {
    let configs = crate::schemas::generated::configs();
    let config = configs.iter().find(|c| {
        c.schema
            .resource_type
            .strip_prefix("aws.")
            .map(|t| t == resource_type)
            .unwrap_or(false)
    });
    let config = match config {
        Some(c) => c,
        None => return,
    };

    let mut resolved = HashMap::new();
    for (key, value) in attributes.iter() {
        if let Some(attr_schema) = config.schema.attributes.get(key.as_str()) {
            if let Some(parts) = attr_schema.attr_type.namespaced_enum_parts() {
                let enum_vals = attr_schema
                    .attr_type
                    .string_enum_parts()
                    .map(|(_, v, _, _)| v);
                let check = |s: &str| {
                    enum_vals.is_some_and(|vals| vals.iter().any(|v| v.eq_ignore_ascii_case(s)))
                };
                if let Some(normalized) =
                    carina_core::utils::normalize_state_enum_value(value, &parts, Some(&check))
                {
                    resolved.insert(key.clone(), normalized);
                }
            }
            // Normalize enum fields within struct (Map) values
            if let carina_core::schema::AttributeType::Struct { fields, .. } =
                &attr_schema.attr_type
                && let Value::Map(map_fields) = value
            {
                let mut normalized_map = map_fields.clone();
                for field in fields {
                    if let Some(parts) = field.field_type.namespaced_enum_parts()
                        && let Some(field_value) = map_fields.get(&field.name)
                    {
                        // Struct field state normalization: bare values only (no dot-check needed)
                        if let Some(normalized) =
                            carina_core::utils::resolve_enum_value(field_value, &parts)
                        {
                            normalized_map.insert(field.name.clone(), normalized);
                        }
                    }
                }
                if normalized_map != *map_fields {
                    resolved.insert(key.clone(), Value::Map(normalized_map));
                }
            }
        }
    }

    for (key, value) in resolved {
        attributes.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_enum_identifiers_namespaced_value() {
        let mut resource = Resource::with_provider("aws", "s3.bucket", "test-bucket");
        resource.set_attr(
            "versioning_status".to_string(),
            Value::String("aws.s3.bucket.VersioningStatus.Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_bare_ident() {
        let mut resource = Resource::with_provider("aws", "s3.bucket", "test-bucket");
        resource.set_attr(
            "versioning_status".to_string(),
            Value::String("Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_typename_value() {
        let mut resource = Resource::with_provider("aws", "s3.bucket", "test-bucket");
        resource.set_attr(
            "object_ownership".to_string(),
            Value::String("ObjectOwnership.BucketOwnerEnforced".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("object_ownership"),
            Some(&Value::String(
                "aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_plain_string() {
        let mut resource = Resource::with_provider("aws", "s3.bucket", "test-bucket");
        resource.set_attr(
            "versioning_status".to_string(),
            Value::String("Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_skips_non_aws() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test");
        resource.set_attr(
            "versioning_status".to_string(),
            Value::String("Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        // Should not be modified since provider is "awscc"
        assert_eq!(
            resources[0].get_attr("versioning_status"),
            Some(&Value::String("Enabled".to_string()))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_with_to_dsl() {
        // ip_protocol has to_dsl that maps "-1" → "all"
        let mut resource =
            Resource::with_provider("aws", "ec2.security_group_ingress", "test-rule");
        resource.set_attr("ip_protocol".to_string(), Value::String("-1".to_string()));
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_with_to_dsl() {
        // Read returns "-1" for ip_protocol, should be normalized to "all" via to_dsl
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("-1".to_string()))]);
        normalize_state_enums("ec2.security_group_ingress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums() {
        let mut attributes = HashMap::from([
            ("bucket".to_string(), Value::String("my-bucket".to_string())),
            (
                "versioning_status".to_string(),
                Value::String("Enabled".to_string()),
            ),
            (
                "object_ownership".to_string(),
                Value::String("BucketOwnerEnforced".to_string()),
            ),
        ]);
        normalize_state_enums("s3.bucket", &mut attributes);
        assert_eq!(
            attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
        assert_eq!(
            attributes.get("object_ownership"),
            Some(&Value::String(
                "aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
            ))
        );
        // Non-enum attributes should not be modified
        assert_eq!(
            attributes.get("bucket"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn test_normalize_state_enums_already_namespaced() {
        let mut attributes = HashMap::from([(
            "versioning_status".to_string(),
            Value::String("aws.s3.bucket.VersioningStatus.Enabled".to_string()),
        )]);
        normalize_state_enums("s3.bucket", &mut attributes);
        // Already namespaced values (contain dots) should not be modified
        assert_eq!(
            attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_ec2_vpc_instance_tenancy() {
        let mut resource = Resource::with_provider("aws", "ec2.vpc", "test-vpc");
        resource.set_attr(
            "instance_tenancy".to_string(),
            Value::String("InstanceTenancy.dedicated".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("instance_tenancy"),
            Some(&Value::String(
                "aws.ec2.vpc.InstanceTenancy.dedicated".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_ec2_security_group_ingress_protocol() {
        let mut resource =
            Resource::with_provider("aws", "ec2.security_group_ingress", "test-rule");
        resource.set_attr(
            "ip_protocol".to_string(),
            Value::String("IpProtocol.tcp".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers(&mut resources);
        assert_eq!(
            resources[0].get_attr("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.tcp".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_ec2_vpc_tenancy() {
        let mut attributes = HashMap::from([(
            "instance_tenancy".to_string(),
            Value::String("default".to_string()),
        )]);
        normalize_state_enums("ec2.vpc", &mut attributes);
        assert_eq!(
            attributes.get("instance_tenancy"),
            Some(&Value::String(
                "aws.ec2.vpc.InstanceTenancy.default".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_ec2_security_group_egress() {
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("-1".to_string()))]);
        normalize_state_enums("ec2.security_group_egress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_egress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_struct_field_enum() {
        let mut inner = HashMap::new();
        inner.insert(
            "hostname_type".to_string(),
            Value::String("ip-name".to_string()),
        );
        inner.insert(
            "enable_resource_name_dns_a_record".to_string(),
            Value::Bool(true),
        );
        let mut attributes = HashMap::from([(
            "private_dns_name_options_on_launch".to_string(),
            Value::Map(inner),
        )]);
        normalize_state_enums("ec2.subnet", &mut attributes);
        if let Some(Value::Map(fields)) = attributes.get("private_dns_name_options_on_launch") {
            assert_eq!(
                fields.get("hostname_type"),
                Some(&Value::String(
                    "aws.ec2.subnet.HostnameType.ip_name".to_string()
                ))
            );
            // Non-enum fields should not be modified
            assert_eq!(
                fields.get("enable_resource_name_dns_a_record"),
                Some(&Value::Bool(true))
            );
        } else {
            panic!("Expected Value::Map for private_dns_name_options_on_launch");
        }
    }

    #[test]
    fn test_normalize_state_enums_ec2_security_group_egress_tcp() {
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("tcp".to_string()))]);
        normalize_state_enums("ec2.security_group_egress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_egress.IpProtocol.tcp".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_vpn_gateway_type_with_dot() {
        // "ipsec.1" contains a dot but is a raw enum value, not a namespaced identifier.
        // The normalizer should recognize it as a valid enum value and namespace it.
        let mut attributes =
            HashMap::from([("type".to_string(), Value::String("ipsec.1".to_string()))]);
        normalize_state_enums("ec2.vpn_gateway", &mut attributes);
        assert_eq!(
            attributes.get("type"),
            Some(&Value::String(
                "aws.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_vpn_gateway_type_already_namespaced() {
        // Already in DSL format — should NOT be double-normalized.
        let mut attributes = HashMap::from([(
            "type".to_string(),
            Value::String("aws.ec2.vpn_gateway.Type.ipsec.1".to_string()),
        )]);
        normalize_state_enums("ec2.vpn_gateway", &mut attributes);
        assert_eq!(
            attributes.get("type"),
            Some(&Value::String(
                "aws.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }
}
