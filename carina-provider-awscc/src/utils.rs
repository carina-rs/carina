//! Utility functions for value normalization and conversion

/// Convert DSL enum value to AWS SDK format
/// e.g., "aws.Region.ap_northeast_1" -> "ap-northeast-1"
/// e.g., "awscc.ec2_ipam.Tier.advanced" -> "advanced"
pub fn convert_enum_value(value: &str) -> String {
    let parts: Vec<&str> = value.split('.').collect();
    let raw_value = match parts.len() {
        2 => {
            if parts[0].chars().next().is_some_and(|c| c.is_uppercase()) {
                parts[1]
            } else {
                return value.to_string();
            }
        }
        3 => {
            let provider = parts[0];
            let type_name = parts[1];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[2]
            } else {
                return value.to_string();
            }
        }
        // 4-part: provider.resource.TypeName.value
        // e.g., "awscc.ec2_ipam.Tier.advanced" -> "advanced"
        4 => {
            let provider = parts[0];
            let type_name = parts[2];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[3]
            } else {
                return value.to_string();
            }
        }
        _ => return value.to_string(),
    };
    raw_value.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_enum_value() {
        assert_eq!(
            convert_enum_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(
            convert_enum_value("Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_enum_value("eu-west-1"), "eu-west-1");
        // 4-part: provider.resource.TypeName.value
        assert_eq!(
            convert_enum_value("awscc.ec2_ipam.Tier.advanced"),
            "advanced"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2_ipam_pool.AddressFamily.IPv4"),
            "IPv4"
        );
        assert_eq!(
            convert_enum_value("awscc.ec2_vpc.InstanceTenancy.default"),
            "default"
        );
    }
}
