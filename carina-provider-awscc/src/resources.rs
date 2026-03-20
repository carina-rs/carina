//! Schema configuration tests for AWS Cloud Control API resources.

#[cfg(test)]
mod tests {
    use crate::schemas::generated::{AwsccSchemaConfig, configs};

    /// Helper to find a config by resource type
    fn get_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
        configs().into_iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("awscc.")
                .map(|t| t == resource_type)
                .unwrap_or(false)
        })
    }

    #[test]
    fn test_get_schema_config() {
        assert!(get_config("ec2.vpc").is_some());
        assert!(get_config("ec2.subnet").is_some());
        assert!(get_config("unknown").is_none());
    }

    #[test]
    fn test_schema_config_aws_type() {
        assert_eq!(
            get_config("ec2.vpc").unwrap().aws_type_name,
            "AWS::EC2::VPC"
        );
        assert_eq!(
            get_config("ec2.subnet").unwrap().aws_type_name,
            "AWS::EC2::Subnet"
        );
        assert_eq!(
            get_config("ec2.security_group_ingress")
                .unwrap()
                .aws_type_name,
            "AWS::EC2::SecurityGroupIngress"
        );
    }

    #[test]
    fn test_schema_config_has_tags() {
        assert!(get_config("ec2.vpc").unwrap().has_tags);
        assert!(get_config("ec2.subnet").unwrap().has_tags);
        assert!(!get_config("ec2.route").unwrap().has_tags);
        assert!(!get_config("ec2.vpc_gateway_attachment").unwrap().has_tags);
    }

    #[test]
    fn test_schema_config_provider_name() {
        let vpc_config = get_config("ec2.vpc").unwrap();
        let cidr_attr = vpc_config.schema.attributes.get("cidr_block").unwrap();
        assert_eq!(cidr_attr.provider_name.as_deref(), Some("CidrBlock"));
        let vpc_id_attr = vpc_config.schema.attributes.get("vpc_id").unwrap();
        assert_eq!(vpc_id_attr.provider_name.as_deref(), Some("VpcId"));
    }

    /// Verify that every `List<Struct>` attribute (both top-level and nested)
    /// has a `block_name` defined. Without `block_name`, the formatter cannot
    /// convert `= [{...}]` syntax into block syntax.
    #[test]
    fn all_list_struct_attributes_have_block_name() {
        use carina_core::schema::{AttributeSchema, AttributeType, StructField};

        /// Collect missing block_names from an AttributeType, recursing into Structs.
        fn check_type(attr_type: &AttributeType, path: &str, missing: &mut Vec<String>) {
            match attr_type {
                AttributeType::Struct { fields, .. } => {
                    for field in fields {
                        check_field(field, path, missing);
                    }
                }
                AttributeType::List { inner, .. } => {
                    check_type(inner, path, missing);
                }
                AttributeType::Map(inner) => {
                    check_type(inner, path, missing);
                }
                _ => {}
            }
        }

        /// Check a StructField: if it is List<Struct>, it must have block_name.
        fn check_field(field: &StructField, parent_path: &str, missing: &mut Vec<String>) {
            let field_path = format!("{}.{}", parent_path, field.name);
            if let AttributeType::List { inner, .. } = &field.field_type
                && matches!(inner.as_ref(), AttributeType::Struct { .. })
                && field.block_name.is_none()
            {
                missing.push(field_path.clone());
            }
            // Recurse into the field type regardless
            check_type(&field.field_type, &field_path, missing);
        }

        /// Check a top-level AttributeSchema: if it is List<Struct>, it must have block_name.
        fn check_attr(attr: &AttributeSchema, resource_type: &str, missing: &mut Vec<String>) {
            let path = format!("{}.{}", resource_type, attr.name);
            if let AttributeType::List { inner, .. } = &attr.attr_type
                && matches!(inner.as_ref(), AttributeType::Struct { .. })
                && attr.block_name.is_none()
            {
                missing.push(path.clone());
            }
            // Recurse into the attribute type regardless
            check_type(&attr.attr_type, &path, missing);
        }

        let mut missing = Vec::new();
        for config in configs() {
            let resource_type = &config.schema.resource_type;
            for attr in config.schema.attributes.values() {
                check_attr(attr, resource_type, &mut missing);
            }
        }

        missing.sort();
        assert!(
            missing.is_empty(),
            "The following List<Struct> attributes are missing block_name:\n{}",
            missing
                .iter()
                .map(|p| format!("  - {}", p))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
