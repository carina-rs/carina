//! AWS-specific type definitions

use carina_core::resource::Value;
use carina_core::schema::AttributeType;

/// Valid AWS regions (in AWS format with hyphens)
const VALID_REGIONS: &[&str] = &[
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-south-1",
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "ca-central-1",
    "sa-east-1",
];

/// AWS region type with custom validation
/// Accepts:
/// - DSL format: aws.Region.ap_northeast_1
/// - AWS string format: "ap-northeast-1"
pub fn aws_region() -> AttributeType {
    AttributeType::Custom {
        name: "Region".to_string(),
        base: Box::new(AttributeType::String),
        validate: |value| {
            if let Value::String(s) = value {
                // Normalize the input to AWS format (hyphens)
                let normalized = normalize_region(s);
                if VALID_REGIONS.contains(&normalized.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "Invalid region '{}', expected one of: {} or DSL format like aws.Region.ap_northeast_1",
                        s,
                        VALID_REGIONS.join(", ")
                    ))
                }
            } else {
                Err("Expected string".to_string())
            }
        },
    }
}

/// Normalize region string to AWS format (hyphens)
/// - "aws.Region.ap_northeast_1" -> "ap-northeast-1"
/// - "ap_northeast_1" -> "ap-northeast-1"
/// - "ap-northeast-1" -> "ap-northeast-1"
fn normalize_region(s: &str) -> String {
    // Extract region part from DSL format (aws.Region.xxx)
    let region_part = if s.contains('.') {
        s.split('.').next_back().unwrap_or(s)
    } else {
        s
    };
    // Convert underscores to hyphens
    region_part.replace('_', "-")
}
