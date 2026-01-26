//! AWS-specific type definitions

use carina_core::schema::AttributeType;

/// AWS region enum type
/// Accepts both DSL format (ap_northeast_1) and AWS format (ap-northeast-1)
pub fn aws_region() -> AttributeType {
    AttributeType::Enum(vec![
        // DSL format (underscores)
        "ap_northeast_1".to_string(),
        "ap_northeast_2".to_string(),
        "ap_northeast_3".to_string(),
        "ap_southeast_1".to_string(),
        "ap_southeast_2".to_string(),
        "ap_south_1".to_string(),
        "us_east_1".to_string(),
        "us_east_2".to_string(),
        "us_west_1".to_string(),
        "us_west_2".to_string(),
        "eu_west_1".to_string(),
        "eu_west_2".to_string(),
        "eu_west_3".to_string(),
        "eu_central_1".to_string(),
        "eu_north_1".to_string(),
        "ca_central_1".to_string(),
        "sa_east_1".to_string(),
        // AWS format (hyphens)
        "ap-northeast-1".to_string(),
        "ap-northeast-2".to_string(),
        "ap-northeast-3".to_string(),
        "ap-southeast-1".to_string(),
        "ap-southeast-2".to_string(),
        "ap-south-1".to_string(),
        "us-east-1".to_string(),
        "us-east-2".to_string(),
        "us-west-1".to_string(),
        "us-west-2".to_string(),
        "eu-west-1".to_string(),
        "eu-west-2".to_string(),
        "eu-west-3".to_string(),
        "eu-central-1".to_string(),
        "eu-north-1".to_string(),
        "ca-central-1".to_string(),
        "sa-east-1".to_string(),
    ])
}
