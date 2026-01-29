//! S3 bucket schema definition

use carina_core::schema::{AttributeSchema, CompletionValue, ResourceSchema, types};

use super::types as aws_types;

/// Returns the schema for S3 buckets
pub fn bucket_schema() -> ResourceSchema {
    ResourceSchema::new("s3.bucket")
        .with_description("An S3 bucket for object storage")
        .attribute(
            AttributeSchema::new("name", aws_types::s3_bucket_name())
                .with_description("Override bucket name (defaults to resource name)"),
        )
        .attribute(
            AttributeSchema::new("region", aws_types::aws_region()).with_description(
                "The AWS region for the bucket (inherited from provider if not specified)",
            ),
        )
        .attribute(
            AttributeSchema::new("acl", aws_types::s3_acl())
                .with_description("The canned ACL for the bucket"),
        )
        .attribute(
            AttributeSchema::new("versioning", aws_types::versioning_status())
                .with_description("Versioning status for the bucket (Enabled or Suspended)")
                .with_completions(vec![
                    CompletionValue::new(
                        "aws.s3.VersioningStatus.Enabled",
                        "Enable versioning for the bucket",
                    ),
                    CompletionValue::new(
                        "aws.s3.VersioningStatus.Suspended",
                        "Suspend versioning for the bucket",
                    ),
                ]),
        )
        .attribute(
            AttributeSchema::new("expiration_days", types::positive_int())
                .with_description("Number of days before objects expire"),
        )
}

/// Returns all S3-related schemas
pub fn schemas() -> Vec<ResourceSchema> {
    vec![bucket_schema()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::Value;
    use std::collections::HashMap;

    #[test]
    fn valid_bucket() {
        let schema = bucket_schema();
        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert(
            "versioning".to_string(),
            Value::String("Enabled".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn valid_bucket_with_optional_name() {
        let schema = bucket_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );

        assert!(schema.validate(&attrs).is_ok());
    }

    #[test]
    fn invalid_bucket_name() {
        let schema = bucket_schema();
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("ab".to_string())); // too short
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );

        assert!(schema.validate(&attrs).is_err());
    }

    #[test]
    fn invalid_region() {
        let schema = bucket_schema();
        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("Region.invalid_region".to_string()),
        );

        assert!(schema.validate(&attrs).is_err());
    }

    #[test]
    fn region_is_optional() {
        let schema = bucket_schema();
        let attrs = HashMap::new();

        // Region is no longer required (inherited from provider)
        let result = schema.validate(&attrs);
        assert!(result.is_ok());
    }

    #[test]
    fn invalid_expiration_days() {
        let schema = bucket_schema();
        let mut attrs = HashMap::new();
        attrs.insert(
            "region".to_string(),
            Value::String("Region.ap_northeast_1".to_string()),
        );
        attrs.insert("expiration_days".to_string(), Value::Int(-1)); // negative

        assert!(schema.validate(&attrs).is_err());
    }
}
