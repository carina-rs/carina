//! Update patch building and resource property parsing.
//!
//! This module builds JSON Patch operations for CloudControl API update requests
//! by comparing current state with desired state.

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, State, Value};
use serde_json::json;

use super::conversion::dsl_value_to_aws;
use crate::schemas::generated::AwsccSchemaConfig;

/// Parse a JSON string from CloudControl API response into a `serde_json::Value`.
///
/// Returns an error instead of silently returning an empty object when the JSON is malformed.
pub(crate) fn parse_resource_properties(props_str: &str) -> ProviderResult<serde_json::Value> {
    serde_json::from_str(props_str)
        .map_err(|e| ProviderError::new("Failed to parse resource properties").with_cause(e))
}

/// Build JSON Patch operations for updating a resource.
///
/// Compares `from` (current state) and `to` (desired state) to generate:
/// - `"add"` operations for attributes present in `to`
/// - `"remove"` operations for attributes present in `from` but absent in `to`
///   (only for non-required, non-create-only attributes with a provider_name)
pub(crate) fn build_update_patches(
    config: &AwsccSchemaConfig,
    from: &State,
    to: &Resource,
) -> Vec<serde_json::Value> {
    let mut patch_ops = Vec::new();
    let resource_type = &to.id.resource_type;

    // Build add operations for attributes that changed between `from` and `to`
    for (dsl_name, attr_schema) in &config.schema.attributes {
        // Skip tags - handled separately below
        if dsl_name == "tags" {
            continue;
        }
        // Skip read-only attributes - they are set by the provider and cannot be updated
        if attr_schema.read_only {
            continue;
        }
        // Skip create-only attributes - they cannot be modified after creation
        if attr_schema.create_only {
            continue;
        }
        if let Some(aws_name) = &attr_schema.provider_name
            && let Some(value) = to.attributes.get(dsl_name.as_str())
            && let Some(aws_value) =
                dsl_value_to_aws(value, &attr_schema.attr_type, resource_type, dsl_name)
        {
            // Skip if the value is unchanged from the current state
            if let Some(from_value) = from.attributes.get(dsl_name)
                && from_value == value
            {
                continue;
            }
            patch_ops.push(json!({
                "op": "add",
                "path": format!("/{}", aws_name),
                "value": aws_value
            }));
        }
    }

    // Build remove operations for attributes present in `from` but absent in `to`
    for (dsl_name, attr_schema) in &config.schema.attributes {
        if dsl_name == "tags" {
            continue;
        }
        // Skip read-only attributes - they are set by the provider and cannot be updated
        if attr_schema.read_only {
            continue;
        }
        // Only generate remove for attributes that:
        // 1. Have a provider_name (so we know the AWS path)
        // 2. Are not required (required attributes cannot be removed)
        // 3. Are not create-only (create-only attributes cannot be changed after creation)
        // 4. Exist in from but not in to
        if let Some(aws_name) = &attr_schema.provider_name
            && !attr_schema.required
            && !attr_schema.create_only
            && from.attributes.contains_key(dsl_name)
            && !to.attributes.contains_key(dsl_name)
        {
            patch_ops.push(json!({
                "op": "remove",
                "path": format!("/{}", aws_name)
            }));
        }
    }

    // Handle tags
    if config.has_tags {
        if let Some(Value::Map(user_tags)) = to.attributes.get("tags") {
            // Skip if tags are unchanged from the current state
            let tags_unchanged = matches!(from.attributes.get("tags"), Some(Value::Map(from_tags)) if from_tags == user_tags);
            if !tags_unchanged {
                let mut tags = Vec::new();
                for (key, value) in user_tags {
                    if let Value::String(v) = value {
                        tags.push(json!({"Key": key, "Value": v}));
                    }
                }
                if !tags.is_empty() {
                    patch_ops.push(json!({"op": "add", "path": "/Tags", "value": tags}));
                }
            }
        } else if from.attributes.contains_key("tags") {
            // Tags existed in from but removed in to: generate remove operation
            patch_ops.push(json!({"op": "remove", "path": "/Tags"}));
        }
    }

    patch_ops
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use carina_core::resource::ResourceId;

    fn get_schema_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
        super::super::get_schema_config(resource_type)
    }

    fn get_vpc_config() -> AwsccSchemaConfig {
        get_schema_config("ec2.vpc").expect("ec2.vpc schema should exist")
    }

    #[test]
    fn test_parse_resource_properties_valid_json() {
        let json_str = r#"{"VpcId": "vpc-123", "CidrBlock": "10.0.0.0/16"}"#;
        let result = parse_resource_properties(json_str);
        assert!(result.is_ok());
        let value = result.unwrap();
        assert_eq!(value["VpcId"], "vpc-123");
        assert_eq!(value["CidrBlock"], "10.0.0.0/16");
    }

    #[test]
    fn test_parse_resource_properties_malformed_json_returns_error() {
        let malformed = r#"{"VpcId": "vpc-123", invalid"#;
        let result = parse_resource_properties(malformed);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Failed to parse resource properties"),
            "Expected error message about parsing, got: {}",
            err.message
        );
    }

    #[test]
    fn test_parse_resource_properties_empty_string_returns_error() {
        let result = parse_resource_properties("");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_update_patches_remove_attribute_absent_in_to() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        let has_remove_instance_tenancy = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/InstanceTenancy")
        });
        assert!(
            has_remove_instance_tenancy,
            "Expected remove patch for /InstanceTenancy, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_remove_tags_absent_in_to() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut tags = HashMap::new();
        tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        from_attrs.insert("tags".to_string(), Value::Map(tags));
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        let has_remove_tags = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/Tags")
        });
        assert!(
            has_remove_tags,
            "Expected remove patch for /Tags, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_no_remove_for_required_attribute() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let to = Resource::new("ec2.vpc", "test");

        let patches = build_update_patches(&config, &from, &to);

        let has_remove_cidr = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/CidrBlock")
        });
        assert!(
            !has_remove_cidr,
            "Should not remove required attribute CidrBlock, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_no_remove_when_both_present() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        let has_remove = patches
            .iter()
            .any(|p| p.get("op").and_then(|v| v.as_str()) == Some("remove"));
        assert!(
            !has_remove,
            "Should not have remove operations when attribute is present in both from and to, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_skip_unchanged_attributes() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        let has_cidr_replace = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("add")
                && p.get("path").and_then(|v| v.as_str()) == Some("/CidrBlock")
        });
        assert!(
            !has_cidr_replace,
            "Should not generate add patch for unchanged attribute /CidrBlock, got: {:?}",
            patches
        );

        let has_tenancy_replace = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("add")
                && p.get("path").and_then(|v| v.as_str()) == Some("/InstanceTenancy")
        });
        assert!(
            has_tenancy_replace,
            "Should generate add patch for changed attribute /InstanceTenancy, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_no_patches_when_identical() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        assert!(
            patches.is_empty(),
            "Should generate no patches when from and to are identical, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_skip_unchanged_tags() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut tags = HashMap::new();
        tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        from_attrs.insert("tags".to_string(), Value::Map(tags.clone()));
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert("tags".to_string(), Value::Map(tags));

        let patches = build_update_patches(&config, &from, &to);

        assert!(
            patches.is_empty(),
            "Should generate no patches when tags are unchanged, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_uses_add_op_not_replace() {
        // CloudControl API JSON Patch: "add" works whether the property exists or not,
        // while "replace" fails if the property doesn't exist in the model.
        // Using "add" is more robust and matches the AWS AWSCC Terraform provider behavior.
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // All value-setting operations should use "add", not "replace"
        for patch in &patches {
            let op = patch.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op != "remove" {
                assert_eq!(
                    op, "add",
                    "Expected 'add' op for value-setting patch, got '{}': {:?}",
                    op, patch
                );
            }
        }
    }

    #[test]
    fn test_build_update_patches_log_group_retention_uses_add() {
        // Regression test for issue #791: logs_log_group in-place update fails
        // because "replace" op fails when the property path doesn't exist in
        // the CloudControl model.
        let config =
            get_schema_config("logs.log_group").expect("logs.log_group schema should exist");
        let id = ResourceId::with_provider("awscc", "logs.log_group", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert("retention_in_days".to_string(), Value::Int(7));
        from_attrs.insert(
            "log_group_name".to_string(),
            Value::String("/carina/test-group".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("logs.log_group", "test");
        to.attributes
            .insert("retention_in_days".to_string(), Value::Int(14));
        to.attributes.insert(
            "log_group_name".to_string(),
            Value::String("/carina/test-group".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        assert_eq!(
            patches.len(),
            1,
            "Should have exactly one patch: {:?}",
            patches
        );
        let patch = &patches[0];
        assert_eq!(
            patch.get("op").and_then(|v| v.as_str()),
            Some("add"),
            "Should use 'add' op for RetentionInDays update"
        );
        assert_eq!(
            patch.get("path").and_then(|v| v.as_str()),
            Some("/RetentionInDays"),
        );
        assert_eq!(patch.get("value"), Some(&serde_json::json!(14)),);
    }

    #[test]
    fn test_build_update_patches_excludes_read_only_properties() {
        // Regression test for issue #806: update patch includes readOnly property Arn
        // CloudFormation rejects patches that include read-only properties like Arn.
        let config =
            get_schema_config("logs.log_group").expect("logs.log_group schema should exist");
        let id = ResourceId::with_provider("awscc", "logs.log_group", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert("retention_in_days".to_string(), Value::Int(7));
        from_attrs.insert(
            "log_group_name".to_string(),
            Value::String("/carina/test-group".to_string()),
        );
        // Arn is a read-only property returned by the API in current state
        from_attrs.insert(
            "arn".to_string(),
            Value::String(
                "arn:aws:logs:ap-northeast-1:123456789012:log-group:/carina/test-group:*"
                    .to_string(),
            ),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("logs.log_group", "test");
        to.attributes
            .insert("retention_in_days".to_string(), Value::Int(14));
        to.attributes.insert(
            "log_group_name".to_string(),
            Value::String("/carina/test-group".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // Should only have the retention_in_days patch, not Arn
        let has_arn_patch = patches
            .iter()
            .any(|p| p.get("path").and_then(|v| v.as_str()) == Some("/Arn"));
        assert!(
            !has_arn_patch,
            "Should not include read-only property Arn in update patches, got: {:?}",
            patches
        );

        // Should still have the retention_in_days patch
        let has_retention_patch = patches
            .iter()
            .any(|p| p.get("path").and_then(|v| v.as_str()) == Some("/RetentionInDays"));
        assert!(
            has_retention_patch,
            "Should include RetentionInDays in update patches, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_excludes_create_only_properties() {
        // Regression test for issue #809: update patch includes create-only property DnsOptions
        // When DnsOptions has nested create-only sub-properties (PrivateDnsPreference,
        // PrivateDnsSpecifiedDomains), the whole DnsOptions should be excluded from patches.
        let config =
            get_schema_config("ec2.vpc_endpoint").expect("ec2.vpc_endpoint schema should exist");
        let id = ResourceId::with_provider("awscc", "ec2.vpc_endpoint", "test");

        let mut from_attrs = HashMap::new();
        from_attrs.insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        from_attrs.insert(
            "service_name".to_string(),
            Value::String("com.amazonaws.ap-northeast-1.s3".to_string()),
        );
        from_attrs.insert(
            "policy_document".to_string(),
            Value::String("{\"Version\":\"2012-10-17\"}".to_string()),
        );
        // DnsOptions is returned by CloudControl API but has create-only sub-properties
        let mut dns_options = HashMap::new();
        dns_options.insert(
            "dns_record_ip_type".to_string(),
            Value::String("awscc.ec2.vpc_endpoint.DnsRecordIpType.ipv4".to_string()),
        );
        from_attrs.insert("dns_options".to_string(), Value::Map(dns_options));
        let from = State::existing(id.clone(), from_attrs);

        // User only specifies policy_document change, no dns_options
        let mut to = Resource::new("ec2.vpc_endpoint", "test");
        to.attributes
            .insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        to.attributes.insert(
            "service_name".to_string(),
            Value::String("com.amazonaws.ap-northeast-1.s3".to_string()),
        );
        to.attributes.insert(
            "policy_document".to_string(),
            Value::String("{\"Version\":\"2012-10-17\",\"Statement\":[]}".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // Should not include DnsOptions in patches (it has create-only sub-properties)
        let has_dns_options_patch = patches
            .iter()
            .any(|p| p.get("path").and_then(|v| v.as_str()) == Some("/DnsOptions"));
        assert!(
            !has_dns_options_patch,
            "Should not include create-only property DnsOptions in update patches, got: {:?}",
            patches
        );

        // Should still have the policy_document patch
        let has_policy_patch = patches
            .iter()
            .any(|p| p.get("path").and_then(|v| v.as_str()) == Some("/PolicyDocument"));
        assert!(
            has_policy_patch,
            "Should include PolicyDocument in update patches, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_delete_retry_constants() {
        use super::super::{
            CREATE_RETRY_INITIAL_DELAY_SECS, CREATE_RETRY_MAX_ATTEMPTS,
            CREATE_RETRY_MAX_DELAY_SECS, DELETE_RETRY_INITIAL_DELAY_SECS,
            DELETE_RETRY_MAX_ATTEMPTS, DELETE_RETRY_MAX_DELAY_SECS,
        };
        assert_eq!(DELETE_RETRY_MAX_ATTEMPTS, 12);
        assert_eq!(DELETE_RETRY_INITIAL_DELAY_SECS, 10);
        assert_eq!(DELETE_RETRY_MAX_DELAY_SECS, 120);
        assert_eq!(DELETE_RETRY_MAX_ATTEMPTS, CREATE_RETRY_MAX_ATTEMPTS);
        assert_eq!(
            DELETE_RETRY_INITIAL_DELAY_SECS,
            CREATE_RETRY_INITIAL_DELAY_SECS
        );
        assert_eq!(DELETE_RETRY_MAX_DELAY_SECS, CREATE_RETRY_MAX_DELAY_SECS);
    }
}
