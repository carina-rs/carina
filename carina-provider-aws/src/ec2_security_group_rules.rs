//! EC2 security group rule shared CRUD operations

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::convert_enum_value;

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Security Group Rule (shared between ingress and egress)
    pub(crate) async fn read_ec2_security_group_rule(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // Look up by rule IDs (may be comma-separated)
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        let mut req = self.ec2_client.describe_security_group_rules();
        for rule_id in &rule_ids {
            req = req.security_group_rule_ids(*rule_id);
        }
        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to describe security group rules")
                .with_cause(e)
                .for_resource(id.clone())
        })?;
        let rules: Vec<_> = result
            .security_group_rules()
            .iter()
            .filter(|rule| rule.is_egress() == Some(!is_ingress))
            .cloned()
            .collect();

        if rules.is_empty() {
            return Ok(State::not_found(id.clone()));
        }

        // Use the first rule for common attributes
        let first_rule = &rules[0];
        let mut attributes = HashMap::new();

        // Auto-generated attribute extraction (common fields)
        if is_ingress {
            Self::extract_ec2_security_group_ingress_attributes(first_rule, &mut attributes);
        } else {
            Self::extract_ec2_security_group_egress_attributes(first_rule, &mut attributes);
        }

        // Override rule IDs with comma-separated values (multi-rule support)
        let rule_ids: Vec<String> = rules
            .iter()
            .filter_map(|r| r.security_group_rule_id().map(String::from))
            .collect();
        let rule_identifier = if !rule_ids.is_empty() {
            attributes.insert(
                "security_group_rule_id".to_string(),
                Value::String(rule_ids.join(",")),
            );
            Some(rule_ids.join(","))
        } else {
            None
        };

        // IPv4 CIDR (CidrIp in schema maps to CidrIpv4 in SDK)
        if let Some(cidr_ip) = first_rule.cidr_ipv4() {
            attributes.insert("cidr_ip".to_string(), Value::String(cidr_ip.to_string()));
        }

        // Referenced security group ID (nested struct, not auto-extracted)
        if let Some(ref_group) = first_rule.referenced_group_info()
            && let Some(group_id) = ref_group.group_id()
        {
            let attr_name = if is_ingress {
                "source_security_group_id"
            } else {
                "destination_security_group_id"
            };
            attributes.insert(attr_name.to_string(), Value::String(group_id.to_string()));
        }

        let state = State::existing(id.clone(), attributes);
        Ok(if let Some(id_str) = rule_identifier {
            state.with_identifier(id_str)
        } else {
            state
        })
    }

    /// Create an EC2 Security Group Rule (shared between ingress and egress)
    pub(crate) async fn create_ec2_security_group_rule(
        &self,
        resource: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let sg_id = match resource.get_attr("group_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Security Group ID (group_id) is required")
                        .for_resource(resource.id.clone()),
                );
            }
        };

        let protocol = match resource.get_attr("ip_protocol") {
            Some(Value::String(s)) => convert_protocol_value(s),
            _ => "-1".to_string(),
        };

        let from_port = match resource.get_attr("from_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let to_port = match resource.get_attr("to_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let cidr_ip = match resource.get_attr("cidr_ip") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let cidr_ipv6 = match resource.get_attr("cidr_ipv6") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let description = match resource.get_attr("description") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let prefix_list_attr = if is_ingress {
            "source_prefix_list_id"
        } else {
            "destination_prefix_list_id"
        };
        let prefix_list_id = match resource.get_attr(prefix_list_attr) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let sg_ref_attr = if is_ingress {
            "source_security_group_id"
        } else {
            "destination_security_group_id"
        };
        let ref_security_group_id = match resource.get_attr(sg_ref_attr) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let mut permission_builder = aws_sdk_ec2::types::IpPermission::builder()
            .ip_protocol(&protocol)
            .from_port(from_port)
            .to_port(to_port);

        // IPv4 CIDR range
        if let Some(ref cidr) = cidr_ip {
            let mut range_builder = aws_sdk_ec2::types::IpRange::builder().cidr_ip(cidr);
            if let Some(ref desc) = description {
                range_builder = range_builder.description(desc);
            }
            permission_builder = permission_builder.ip_ranges(range_builder.build());
        }

        // IPv6 CIDR range
        if let Some(ref cidr_v6) = cidr_ipv6 {
            let mut range_builder = aws_sdk_ec2::types::Ipv6Range::builder().cidr_ipv6(cidr_v6);
            if let Some(ref desc) = description {
                range_builder = range_builder.description(desc);
            }
            permission_builder = permission_builder.ipv6_ranges(range_builder.build());
        }

        // Prefix list
        if let Some(ref pl_id) = prefix_list_id {
            let mut pl_builder = aws_sdk_ec2::types::PrefixListId::builder().prefix_list_id(pl_id);
            if let Some(ref desc) = description {
                pl_builder = pl_builder.description(desc);
            }
            permission_builder = permission_builder.prefix_list_ids(pl_builder.build());
        }

        // Security group reference
        if let Some(ref ref_sg_id) = ref_security_group_id {
            let mut pair_builder =
                aws_sdk_ec2::types::UserIdGroupPair::builder().group_id(ref_sg_id);
            if let Some(ref desc) = description {
                pair_builder = pair_builder.description(desc);
            }
            permission_builder = permission_builder.user_id_group_pairs(pair_builder.build());
        }

        let permission = permission_builder.build();

        let rule_ids: Vec<String> = if is_ingress {
            let result = self
                .ec2_client
                .authorize_security_group_ingress()
                .group_id(&sg_id)
                .ip_permissions(permission)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to create ingress rule")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;

            result
                .security_group_rules()
                .iter()
                .filter_map(|r| r.security_group_rule_id().map(String::from))
                .collect()
        } else {
            let result = self
                .ec2_client
                .authorize_security_group_egress()
                .group_id(&sg_id)
                .ip_permissions(permission)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to create egress rule")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;

            result
                .security_group_rules()
                .iter()
                .filter_map(|r| r.security_group_rule_id().map(String::from))
                .collect()
        };

        // Read back using rule IDs (reliable identifier)
        let identifier = rule_ids.join(",");
        self.read_ec2_security_group_rule(
            &resource.id,
            if identifier.is_empty() {
                None
            } else {
                Some(&identifier)
            },
            is_ingress,
        )
        .await
    }

    /// Update an EC2 Security Group Rule (rules are immutable, so recreate)
    pub(crate) async fn update_ec2_security_group_rule(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        // Security group rules are immutable - delete and recreate
        self.delete_ec2_security_group_rule(id.clone(), identifier, is_ingress)
            .await?;
        self.create_ec2_security_group_rule(to, is_ingress).await
    }

    /// Delete an EC2 Security Group Rule (deletes all rules by identifier)
    pub(crate) async fn delete_ec2_security_group_rule(
        &self,
        id: ResourceId,
        identifier: &str,
        is_ingress: bool,
    ) -> ProviderResult<()> {
        // identifier is comma-separated rule IDs (e.g., "sgr-123,sgr-456")
        let rule_ids: Vec<&str> = identifier.split(',').collect();

        // Look up the rules to get the security group ID
        let mut req = self.ec2_client.describe_security_group_rules();
        for rule_id in &rule_ids {
            req = req.security_group_rule_ids(*rule_id);
        }
        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to describe security group rules")
                .with_cause(e)
                .for_resource(id.clone())
        })?;

        let rules = result.security_group_rules();
        if rules.is_empty() {
            return Err(
                ProviderError::new("Security Group Rule not found").for_resource(id.clone())
            );
        }

        let sg_id = rules[0].group_id().ok_or_else(|| {
            ProviderError::new("Rule has no security group ID").for_resource(id.clone())
        })?;

        // Delete all rules at once
        if is_ingress {
            let mut request = self
                .ec2_client
                .revoke_security_group_ingress()
                .group_id(sg_id);
            for rule_id in &rule_ids {
                request = request.security_group_rule_ids(*rule_id);
            }
            request.send().await.map_err(|e| {
                ProviderError::new("Failed to delete ingress rules")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        } else {
            let mut request = self
                .ec2_client
                .revoke_security_group_egress()
                .group_id(sg_id);
            for rule_id in &rule_ids {
                request = request.security_group_rule_ids(*rule_id);
            }
            request.send().await.map_err(|e| {
                ProviderError::new("Failed to delete egress rules")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        Ok(())
    }
}

/// Convert protocol value from DSL format to AWS format
/// - aws.Protocol.tcp / Protocol.tcp / tcp -> tcp
/// - aws.Protocol.all / Protocol.all / all / -1 -> -1
pub(crate) fn convert_protocol_value(value: &str) -> String {
    // First convert DSL enum format to raw value
    let raw = convert_enum_value(value);

    // Handle special case: "all" means "-1" (all protocols)
    if raw == "all" { "-1".to_string() } else { raw }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- convert_protocol_value tests ---

    #[test]
    fn test_convert_protocol_value_tcp() {
        assert_eq!(convert_protocol_value("tcp"), "tcp");
    }

    #[test]
    fn test_convert_protocol_value_udp() {
        assert_eq!(convert_protocol_value("udp"), "udp");
    }

    #[test]
    fn test_convert_protocol_value_all_keyword() {
        assert_eq!(convert_protocol_value("all"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_minus_one() {
        assert_eq!(convert_protocol_value("-1"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_dsl_format_tcp() {
        assert_eq!(convert_protocol_value("aws.Protocol.tcp"), "tcp");
    }

    #[test]
    fn test_convert_protocol_value_dsl_format_all() {
        assert_eq!(convert_protocol_value("aws.Protocol.all"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_short_dsl_format() {
        assert_eq!(convert_protocol_value("Protocol.tcp"), "tcp");
    }

    // --- Route composite identifier parsing tests ---

    #[test]
    fn test_route_identifier_parsing() {
        let identifier = "rtb-12345678|0.0.0.0/0";
        let (route_table_id, destination) = identifier.split_once('|').unwrap();
        assert_eq!(route_table_id, "rtb-12345678");
        assert_eq!(destination, "0.0.0.0/0");
    }

    #[test]
    fn test_route_identifier_parsing_no_separator() {
        let identifier = "rtb-12345678";
        assert_eq!(identifier.split_once('|'), None);
    }

    #[test]
    fn test_route_identifier_parsing_ipv6_destination() {
        let identifier = "rtb-12345678|::/0";
        let (route_table_id, destination) = identifier.split_once('|').unwrap();
        assert_eq!(route_table_id, "rtb-12345678");
        assert_eq!(destination, "::/0");
    }

    // --- Security group rule referenced group extraction ---

    #[test]
    fn test_security_group_rule_referenced_group() {
        let ref_group = aws_sdk_ec2::types::ReferencedSecurityGroup::builder()
            .group_id("sg-ref-12345678")
            .build();
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-12345678")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(443)
            .to_port(443)
            .referenced_group_info(ref_group)
            .build();

        // Replicate logic from read_ec2_security_group_rule for ingress
        let mut attributes = HashMap::new();
        if let Some(ref_g) = rule.referenced_group_info()
            && let Some(group_id) = ref_g.group_id()
        {
            attributes.insert(
                "source_security_group_id".to_string(),
                Value::String(group_id.to_string()),
            );
        }

        assert_eq!(
            attributes.get("source_security_group_id"),
            Some(&Value::String("sg-ref-12345678".to_string()))
        );
    }

    #[test]
    fn test_security_group_rule_cidr_ipv4() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-12345678")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(80)
            .to_port(80)
            .cidr_ipv4("10.0.0.0/8")
            .build();

        // Replicate logic from read_ec2_security_group_rule
        let mut attributes = HashMap::new();
        if let Some(cidr_ip) = rule.cidr_ipv4() {
            attributes.insert("cidr_ip".to_string(), Value::String(cidr_ip.to_string()));
        }

        assert_eq!(
            attributes.get("cidr_ip"),
            Some(&Value::String("10.0.0.0/8".to_string()))
        );
    }

    // --- Security group rule is_egress filtering ---

    #[test]
    fn test_security_group_rule_is_egress_filtering() {
        let ingress_rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-ingress")
            .is_egress(false)
            .build();
        let egress_rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-egress")
            .is_egress(true)
            .build();

        let rules = [ingress_rule, egress_rule];

        // Filter for ingress (is_ingress=true means is_egress should be false)
        let ingress_filtered: Vec<_> = rules
            .iter()
            .filter(|rule| rule.is_egress() == Some(false))
            .collect();
        assert_eq!(ingress_filtered.len(), 1);
        assert_eq!(
            ingress_filtered[0].security_group_rule_id(),
            Some("sgr-ingress")
        );

        // Filter for egress (is_ingress=false means is_egress should be true)
        let egress_filtered: Vec<_> = rules
            .iter()
            .filter(|rule| rule.is_egress() == Some(true))
            .collect();
        assert_eq!(egress_filtered.len(), 1);
        assert_eq!(
            egress_filtered[0].security_group_rule_id(),
            Some("sgr-egress")
        );
    }

    // --- Security group rule comma-separated identifiers ---

    #[test]
    fn test_security_group_rule_comma_separated_ids() {
        // Tests the comma-separated rule ID pattern used in multi-rule support
        let identifier = "sgr-111,sgr-222,sgr-333";
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        assert_eq!(rule_ids.len(), 3);
        assert_eq!(rule_ids[0], "sgr-111");
        assert_eq!(rule_ids[1], "sgr-222");
        assert_eq!(rule_ids[2], "sgr-333");
    }

    #[test]
    fn test_security_group_rule_single_id() {
        let identifier = "sgr-111";
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        assert_eq!(rule_ids.len(), 1);
        assert_eq!(rule_ids[0], "sgr-111");
    }
}
