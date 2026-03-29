//! Auto-generated provider boilerplate
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   ./carina-provider-aws/scripts/generate-provider.sh

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;

// ===== Generated Methods on AwsProvider =====

impl AwsProvider {
    /// Delete ec2.vpc (generated)
    pub(crate) async fn delete_ec2_vpc(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_vpc()
            .vpc_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete vpc")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Delete ec2.subnet (generated)
    pub(crate) async fn delete_ec2_subnet(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_subnet()
            .subnet_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete subnet")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Delete ec2.route_table (generated)
    pub(crate) async fn delete_ec2_route_table(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_route_table()
            .route_table_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete route table")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Delete ec2.security_group (generated)
    pub(crate) async fn delete_ec2_security_group(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_security_group()
            .group_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete security group")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    // Note: delete_s3_bucket is manually implemented in services/s3/bucket.rs
    // to support lifecycle.force_delete (emptying bucket before deletion).

    /// Update ec2.internet_gateway: apply tag changes and read back (generated)
    pub(crate) async fn update_ec2_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;
        self.read_ec2_internet_gateway(&id, Some(identifier)).await
    }

    /// Update ec2.route_table: apply tag changes and read back (generated)
    pub(crate) async fn update_ec2_route_table(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;
        self.read_ec2_route_table(&id, Some(identifier)).await
    }

    /// Update ec2.security_group: apply tag changes and read back (generated)
    pub(crate) async fn update_ec2_security_group(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;
        self.read_ec2_security_group(&id, Some(identifier)).await
    }

    /// Read s3.bucket GetBucketVersioning (generated)
    pub(crate) async fn read_s3_bucket_versioning(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) -> ProviderResult<()> {
        let output = self
            .s3_client
            .get_bucket_versioning()
            .bucket(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to read s3.bucket GetBucketVersioning")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        let value = output
            .status()
            .map(|v| v.as_str().to_string())
            .unwrap_or_else(|| "Suspended".to_string());
        attributes.insert("versioning_status".to_string(), Value::String(value));
        Ok(())
    }

    /// Write s3.bucket PutBucketVersioning (generated)
    pub(crate) async fn write_s3_bucket_versioning(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
        let mut builder = VersioningConfiguration::builder();
        let mut has_changes = false;
        if let Some(Value::String(val)) = attributes.get("versioning_status") {
            let normalized = extract_enum_value(val);
            builder = builder.status(BucketVersioningStatus::from(normalized));
            has_changes = true;
        }
        if has_changes {
            let config = builder.build();
            self.s3_client
                .put_bucket_versioning()
                .bucket(identifier)
                .versioning_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to put bucket versioning")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }
        Ok(())
    }

    /// Extract iam.role attributes from SDK response type (generated)
    pub(crate) fn extract_iam_role_attributes(
        obj: &aws_sdk_iam::types::Role,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        // arn, path, role_id, role_name return &str (always present)
        let arn = obj.arn();
        if !arn.is_empty() {
            attributes.insert("arn".to_string(), Value::String(arn.to_string()));
        }
        if let Some(v) = obj.assume_role_policy_document() {
            // The SDK URL-encodes the policy document
            let decoded = urlencoding::decode(v).unwrap_or_else(|_| v.into());
            // Convert JSON string to Value::Map with snake_case keys for struct comparison
            let policy_value = crate::services::iam::role::iam_policy_json_to_value(&decoded)
                .unwrap_or_else(|_| {
                    // Fallback to raw string if JSON parsing fails
                    Value::String(decoded.into_owned())
                });
            attributes.insert("assume_role_policy_document".to_string(), policy_value);
        }
        if let Some(v) = obj.description() {
            attributes.insert("description".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.max_session_duration() {
            attributes.insert("max_session_duration".to_string(), Value::Int(v as i64));
        }
        let path = obj.path();
        if !path.is_empty() {
            attributes.insert("path".to_string(), Value::String(path.to_string()));
        }
        let role_id = obj.role_id();
        if !role_id.is_empty() {
            attributes.insert("role_id".to_string(), Value::String(role_id.to_string()));
        }
        let role_name = obj.role_name();
        if !role_name.is_empty() {
            attributes.insert(
                "role_name".to_string(),
                Value::String(role_name.to_string()),
            );
            Some(role_name.to_string())
        } else {
            None
        }
    }

    /// Extract ec2.eip attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_eip_attributes(
        obj: &aws_sdk_ec2::types::Address,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.allocation_id() {
            attributes.insert("allocation_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.domain() {
            attributes.insert("domain".to_string(), Value::String(v.as_str().to_string()));
        }
        if let Some(v) = obj.public_ip() {
            attributes.insert("public_ip".to_string(), Value::String(v.to_string()));
        }
        obj.allocation_id().map(String::from)
    }

    /// Extract ec2.nat_gateway attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_nat_gateway_attributes(
        obj: &aws_sdk_ec2::types::NatGateway,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        // Extract allocation_id from the first NAT gateway address
        if let Some(addr) = obj.nat_gateway_addresses().first()
            && let Some(v) = addr.allocation_id()
        {
            attributes.insert("allocation_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.connectivity_type() {
            attributes.insert(
                "connectivity_type".to_string(),
                Value::String(v.as_str().to_string()),
            );
        }
        if let Some(v) = obj.nat_gateway_id() {
            attributes.insert("nat_gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.subnet_id() {
            attributes.insert("subnet_id".to_string(), Value::String(v.to_string()));
        }
        obj.nat_gateway_id().map(String::from)
    }

    /// Extract ec2.vpc attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_vpc_attributes(
        obj: &aws_sdk_ec2::types::Vpc,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.cidr_block() {
            attributes.insert("cidr_block".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.instance_tenancy() {
            attributes.insert(
                "instance_tenancy".to_string(),
                Value::String(v.as_str().to_string()),
            );
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        obj.vpc_id().map(String::from)
    }

    /// Extract ec2.subnet attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_subnet_attributes(
        obj: &aws_sdk_ec2::types::Subnet,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.assign_ipv6_address_on_creation() {
            attributes.insert(
                "assign_ipv6_address_on_creation".to_string(),
                Value::Bool(v),
            );
        }
        if let Some(v) = obj.availability_zone() {
            attributes.insert(
                "availability_zone".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.availability_zone_id() {
            attributes.insert(
                "availability_zone_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.cidr_block() {
            attributes.insert("cidr_block".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.enable_dns64() {
            attributes.insert("enable_dns64".to_string(), Value::Bool(v));
        }
        if let Some(v) = obj.enable_lni_at_device_index() {
            attributes.insert(
                "enable_lni_at_device_index".to_string(),
                Value::Int(v as i64),
            );
        }
        if let Some(v) = obj.ipv6_native() {
            attributes.insert("ipv6_native".to_string(), Value::Bool(v));
        }
        if let Some(v) = obj.map_public_ip_on_launch() {
            attributes.insert("map_public_ip_on_launch".to_string(), Value::Bool(v));
        }
        if let Some(v) = obj.outpost_arn() {
            attributes.insert("outpost_arn".to_string(), Value::String(v.to_string()));
        }
        if let Some(dns_opts) = obj.private_dns_name_options_on_launch() {
            let mut fields = HashMap::new();
            if let Some(ht) = dns_opts.hostname_type() {
                fields.insert(
                    "hostname_type".to_string(),
                    Value::String(ht.as_str().to_string()),
                );
            }
            if let Some(v) = dns_opts.enable_resource_name_dns_a_record() {
                fields.insert(
                    "enable_resource_name_dns_a_record".to_string(),
                    Value::Bool(v),
                );
            }
            if let Some(v) = dns_opts.enable_resource_name_dns_aaaa_record() {
                fields.insert(
                    "enable_resource_name_dns_aaaa_record".to_string(),
                    Value::Bool(v),
                );
            }
            if !fields.is_empty() {
                attributes.insert(
                    "private_dns_name_options_on_launch".to_string(),
                    Value::Map(fields),
                );
            }
        }
        if let Some(v) = obj.subnet_id() {
            attributes.insert("subnet_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        obj.subnet_id().map(String::from)
    }

    /// Extract ec2.internet_gateway attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_internet_gateway_attributes(
        obj: &aws_sdk_ec2::types::InternetGateway,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.internet_gateway_id() {
            attributes.insert(
                "internet_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        obj.internet_gateway_id().map(String::from)
    }

    /// Extract ec2.route_table attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_route_table_attributes(
        obj: &aws_sdk_ec2::types::RouteTable,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.route_table_id() {
            attributes.insert("route_table_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        obj.route_table_id().map(String::from)
    }

    /// Extract ec2.route attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_route_attributes(
        obj: &aws_sdk_ec2::types::Route,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.destination_cidr_block() {
            attributes.insert(
                "destination_cidr_block".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.gateway_id() {
            attributes.insert("gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.nat_gateway_id() {
            attributes.insert("nat_gateway_id".to_string(), Value::String(v.to_string()));
        }
        None
    }

    /// Extract ec2.security_group attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_security_group_attributes(
        obj: &aws_sdk_ec2::types::SecurityGroup,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.description() {
            attributes.insert("description".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.group_id() {
            attributes.insert("group_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.group_name() {
            attributes.insert("group_name".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        obj.group_id().map(String::from)
    }

    /// Extract ec2.security_group_ingress attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_security_group_ingress_attributes(
        obj: &aws_sdk_ec2::types::SecurityGroupRule,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.cidr_ipv6() {
            attributes.insert("cidr_ipv6".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.description() {
            attributes.insert("description".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.from_port() {
            attributes.insert("from_port".to_string(), Value::Int(v as i64));
        }
        if let Some(v) = obj.group_id() {
            attributes.insert("group_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.ip_protocol() {
            attributes.insert("ip_protocol".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.security_group_rule_id() {
            attributes.insert(
                "security_group_rule_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.prefix_list_id() {
            attributes.insert(
                "source_prefix_list_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.to_port() {
            attributes.insert("to_port".to_string(), Value::Int(v as i64));
        }
        obj.security_group_rule_id().map(String::from)
    }

    /// Extract ec2.vpc_endpoint attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_vpc_endpoint_attributes(
        obj: &aws_sdk_ec2::types::VpcEndpoint,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.vpc_endpoint_id() {
            attributes.insert("vpc_endpoint_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.vpc_endpoint_type() {
            attributes.insert(
                "vpc_endpoint_type".to_string(),
                Value::String(v.as_str().to_string()),
            );
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.service_name() {
            attributes.insert("service_name".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.private_dns_enabled() {
            attributes.insert("private_dns_enabled".to_string(), Value::Bool(v));
        }
        if let Some(v) = obj.policy_document() {
            // Try to parse the policy document JSON into a Value::Map
            let policy_value = crate::services::iam::role::iam_policy_json_to_value(v)
                .unwrap_or_else(|_| Value::String(v.to_string()));
            attributes.insert("policy_document".to_string(), policy_value);
        }
        {
            let ids = obj.route_table_ids();
            if !ids.is_empty() {
                let list: Vec<Value> = ids.iter().map(|s| Value::String(s.to_string())).collect();
                attributes.insert("route_table_ids".to_string(), Value::List(list));
            }
        }
        {
            let ids = obj.subnet_ids();
            if !ids.is_empty() {
                let list: Vec<Value> = ids.iter().map(|s| Value::String(s.to_string())).collect();
                attributes.insert("subnet_ids".to_string(), Value::List(list));
            }
        }
        // Extract security group IDs from groups
        {
            let groups = obj.groups();
            if !groups.is_empty() {
                let list: Vec<Value> = groups
                    .iter()
                    .filter_map(|g| g.group_id().map(|id| Value::String(id.to_string())))
                    .collect();
                if !list.is_empty() {
                    attributes.insert("security_group_ids".to_string(), Value::List(list));
                }
            }
        }
        obj.vpc_endpoint_id().map(String::from)
    }

    /// Extract ec2.flow_log attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_flow_log_attributes(
        obj: &aws_sdk_ec2::types::FlowLog,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.flow_log_id() {
            attributes.insert("flow_log_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.resource_id() {
            attributes.insert("resource_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.traffic_type() {
            attributes.insert(
                "traffic_type".to_string(),
                Value::String(v.as_str().to_string()),
            );
        }
        if let Some(v) = obj.log_destination_type() {
            attributes.insert(
                "log_destination_type".to_string(),
                Value::String(v.as_str().to_string()),
            );
        }
        if let Some(v) = obj.log_destination() {
            attributes.insert("log_destination".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.log_group_name() {
            attributes.insert("log_group_name".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.deliver_logs_permission_arn() {
            attributes.insert(
                "deliver_logs_permission_arn".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.log_format() {
            attributes.insert("log_format".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.max_aggregation_interval() {
            attributes.insert("max_aggregation_interval".to_string(), Value::Int(v as i64));
        }
        if let Some(v) = obj.flow_log_status()
            && v == "ACTIVE"
        {
            // Only extract resource_type for active flow logs
            if let Some(rt) = obj.resource_id() {
                let resource_type_str = if rt.starts_with("vpc-") {
                    "VPC"
                } else if rt.starts_with("subnet-") {
                    "Subnet"
                } else if rt.starts_with("eni-") {
                    "NetworkInterface"
                } else {
                    ""
                };
                if !resource_type_str.is_empty() {
                    attributes.insert(
                        "resource_type".to_string(),
                        Value::String(resource_type_str.to_string()),
                    );
                }
            }
        }
        obj.flow_log_id().map(String::from)
    }

    /// Extract ec2.vpn_gateway attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_vpn_gateway_attributes(
        obj: &aws_sdk_ec2::types::VpnGateway,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.vpn_gateway_id() {
            attributes.insert("vpn_gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.r#type() {
            attributes.insert("type".to_string(), Value::String(v.as_str().to_string()));
        }
        if let Some(v) = obj.amazon_side_asn() {
            attributes.insert("amazon_side_asn".to_string(), Value::Int(v));
        }
        obj.vpn_gateway_id().map(String::from)
    }

    /// Extract ec2.transit_gateway attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_transit_gateway_attributes(
        obj: &aws_sdk_ec2::types::TransitGateway,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.transit_gateway_id() {
            attributes.insert(
                "transit_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.description() {
            attributes.insert("description".to_string(), Value::String(v.to_string()));
        }
        if let Some(opts) = obj.options() {
            if let Some(v) = opts.amazon_side_asn() {
                attributes.insert("amazon_side_asn".to_string(), Value::Int(v));
            }
            if let Some(v) = opts.auto_accept_shared_attachments() {
                attributes.insert(
                    "auto_accept_shared_attachments".to_string(),
                    Value::String(v.as_str().to_string()),
                );
            }
            if let Some(v) = opts.default_route_table_association() {
                attributes.insert(
                    "default_route_table_association".to_string(),
                    Value::String(v.as_str().to_string()),
                );
            }
            if let Some(v) = opts.default_route_table_propagation() {
                attributes.insert(
                    "default_route_table_propagation".to_string(),
                    Value::String(v.as_str().to_string()),
                );
            }
            if let Some(v) = opts.dns_support() {
                attributes.insert(
                    "dns_support".to_string(),
                    Value::String(v.as_str().to_string()),
                );
            }
            if let Some(v) = opts.vpn_ecmp_support() {
                attributes.insert(
                    "vpn_ecmp_support".to_string(),
                    Value::String(v.as_str().to_string()),
                );
            }
        }
        obj.transit_gateway_id().map(String::from)
    }

    /// Extract ec2.transit_gateway_attachment attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_transit_gateway_attachment_attributes(
        obj: &aws_sdk_ec2::types::TransitGatewayVpcAttachment,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.transit_gateway_attachment_id() {
            attributes.insert(
                "transit_gateway_attachment_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.transit_gateway_id() {
            attributes.insert(
                "transit_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.vpc_id() {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        {
            let ids = obj.subnet_ids();
            if !ids.is_empty() {
                let list: Vec<Value> = ids.iter().map(|s| Value::String(s.to_string())).collect();
                attributes.insert("subnet_ids".to_string(), Value::List(list));
            }
        }
        obj.transit_gateway_attachment_id().map(String::from)
    }

    /// Extract ec2.vpc_peering_connection attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_vpc_peering_connection_attributes(
        obj: &aws_sdk_ec2::types::VpcPeeringConnection,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.vpc_peering_connection_id() {
            attributes.insert(
                "vpc_peering_connection_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(requester) = obj.requester_vpc_info()
            && let Some(v) = requester.vpc_id()
        {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(accepter) = obj.accepter_vpc_info() {
            if let Some(v) = accepter.vpc_id() {
                attributes.insert("peer_vpc_id".to_string(), Value::String(v.to_string()));
            }
            if let Some(v) = accepter.owner_id() {
                attributes.insert("peer_owner_id".to_string(), Value::String(v.to_string()));
            }
            if let Some(v) = accepter.region() {
                attributes.insert("peer_region".to_string(), Value::String(v.to_string()));
            }
        }
        obj.vpc_peering_connection_id().map(String::from)
    }

    /// Extract ec2.egress_only_internet_gateway attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_egress_only_internet_gateway_attributes(
        obj: &aws_sdk_ec2::types::EgressOnlyInternetGateway,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.egress_only_internet_gateway_id() {
            attributes.insert(
                "egress_only_internet_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        // Extract vpc_id from attachments
        if let Some(att) = obj.attachments().first()
            && let Some(v) = att.vpc_id()
        {
            attributes.insert("vpc_id".to_string(), Value::String(v.to_string()));
        }
        obj.egress_only_internet_gateway_id().map(String::from)
    }

    /// Extract ec2.security_group_egress attributes from SDK response type (generated)
    pub(crate) fn extract_ec2_security_group_egress_attributes(
        obj: &aws_sdk_ec2::types::SecurityGroupRule,
        attributes: &mut HashMap<String, Value>,
    ) -> Option<String> {
        if let Some(v) = obj.cidr_ipv6() {
            attributes.insert("cidr_ipv6".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.description() {
            attributes.insert("description".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.prefix_list_id() {
            attributes.insert(
                "destination_prefix_list_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.from_port() {
            attributes.insert("from_port".to_string(), Value::Int(v as i64));
        }
        if let Some(v) = obj.group_id() {
            attributes.insert("group_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.ip_protocol() {
            attributes.insert("ip_protocol".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.security_group_rule_id() {
            attributes.insert(
                "security_group_rule_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.to_port() {
            attributes.insert("to_port".to_string(), Value::Int(v as i64));
        }
        obj.security_group_rule_id().map(String::from)
    }
}
