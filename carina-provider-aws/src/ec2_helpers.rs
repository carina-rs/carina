//! EC2 helper functions for tags and security group rules

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::convert_enum_value;

use crate::AwsProvider;

impl AwsProvider {
    /// Extract tags from EC2 tag list into a Value::Map
    pub(crate) fn ec2_tags_to_value(tags: &[aws_sdk_ec2::types::Tag]) -> Option<Value> {
        let mut tag_map = HashMap::new();
        for tag in tags {
            if let (Some(key), Some(value)) = (tag.key(), tag.value()) {
                tag_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if tag_map.is_empty() {
            None
        } else {
            Some(Value::Map(tag_map))
        }
    }

    /// Build EC2 Tag list from Value::Map
    pub(crate) fn value_to_ec2_tags(value: &Value) -> Vec<aws_sdk_ec2::types::Tag> {
        let mut tags = Vec::new();
        if let Value::Map(map) = value {
            for (key, val) in map {
                if let Value::String(v) = val {
                    tags.push(aws_sdk_ec2::types::Tag::builder().key(key).value(v).build());
                }
            }
        }
        tags
    }

    /// Apply tags to an EC2 resource
    ///
    /// When `from_attributes` is provided, tags that exist in `from` but not in `to`
    /// will be deleted from the resource.
    pub(crate) async fn apply_ec2_tags(
        &self,
        resource_id: &ResourceId,
        ec2_resource_id: &str,
        attributes: &HashMap<String, Value>,
        from_attributes: Option<&HashMap<String, Value>>,
    ) -> ProviderResult<()> {
        // Delete tags that were removed (present in from but not in to)
        if let Some(from_attrs) = from_attributes {
            let old_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(old_map)) = from_attrs.get("tags") {
                    old_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let new_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(new_map)) = attributes.get("tags") {
                    new_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let removed_keys: Vec<&String> = old_keys.difference(&new_keys).copied().collect();
            if !removed_keys.is_empty() {
                let mut req = self.ec2_client.delete_tags().resources(ec2_resource_id);
                for key in removed_keys {
                    req = req.tags(aws_sdk_ec2::types::Tag::builder().key(key.as_str()).build());
                }
                req.send().await.map_err(|e| {
                    ProviderError::new("Failed to delete tags")
                        .with_cause(e)
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        // Add/update tags
        if let Some(tag_value) = attributes.get("tags") {
            let tags = Self::value_to_ec2_tags(tag_value);
            if !tags.is_empty() {
                let mut req = self.ec2_client.create_tags().resources(ec2_resource_id);
                for tag in tags {
                    req = req.tags(tag);
                }
                req.send().await.map_err(|e| {
                    ProviderError::new("Failed to tag resource")
                        .with_cause(e)
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        Ok(())
    }

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
        let sg_id = match resource.attributes.get("group_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Security Group ID (group_id) is required")
                        .for_resource(resource.id.clone()),
                );
            }
        };

        let protocol = match resource.attributes.get("ip_protocol") {
            Some(Value::String(s)) => convert_protocol_value(s),
            _ => "-1".to_string(),
        };

        let from_port = match resource.attributes.get("from_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let to_port = match resource.attributes.get("to_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let cidr_ip = match resource.attributes.get("cidr_ip") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let cidr_ipv6 = match resource.attributes.get("cidr_ipv6") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let description = match resource.attributes.get("description") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let prefix_list_attr = if is_ingress {
            "source_prefix_list_id"
        } else {
            "destination_prefix_list_id"
        };
        let prefix_list_id = match resource.attributes.get(prefix_list_attr) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let sg_ref_attr = if is_ingress {
            "source_security_group_id"
        } else {
            "destination_security_group_id"
        };
        let ref_security_group_id = match resource.attributes.get(sg_ref_attr) {
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

    // --- ec2_tags_to_value tests ---

    #[test]
    fn test_ec2_tags_to_value_empty() {
        let tags: Vec<aws_sdk_ec2::types::Tag> = vec![];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    #[test]
    fn test_ec2_tags_to_value_single_tag() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("my-resource")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        assert!(result.is_some());
        if let Some(Value::Map(map)) = result {
            assert_eq!(
                map.get("Name"),
                Some(&Value::String("my-resource".to_string()))
            );
        } else {
            panic!("Expected Value::Map");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_multiple_tags() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("test")
                .build(),
            aws_sdk_ec2::types::Tag::builder()
                .key("Environment")
                .value("production")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        if let Some(Value::Map(map)) = result {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("Name"), Some(&Value::String("test".to_string())));
            assert_eq!(
                map.get("Environment"),
                Some(&Value::String("production".to_string()))
            );
        } else {
            panic!("Expected Value::Map with 2 entries");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_missing_key_or_value() {
        // Tag with no key set
        let tags = vec![aws_sdk_ec2::types::Tag::builder().build()];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    // --- value_to_ec2_tags tests ---

    #[test]
    fn test_value_to_ec2_tags_from_map() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Env".to_string(), Value::String("prod".to_string())),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 2);
        // Check both tags exist (order not guaranteed from HashMap)
        let tag_map: HashMap<String, String> = tags
            .iter()
            .map(|t| {
                (
                    t.key().unwrap_or("").to_string(),
                    t.value().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(tag_map.get("Name"), Some(&"test".to_string()));
        assert_eq!(tag_map.get("Env"), Some(&"prod".to_string()));
    }

    #[test]
    fn test_value_to_ec2_tags_non_map_value() {
        let value = Value::String("not a map".to_string());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
    }

    #[test]
    fn test_value_to_ec2_tags_non_string_values_skipped() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Count".to_string(), Value::Int(42)),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].key(), Some("Name"));
        assert_eq!(tags[0].value(), Some("test"));
    }

    #[test]
    fn test_value_to_ec2_tags_empty_map() {
        let value = Value::Map(HashMap::new());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
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
