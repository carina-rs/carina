//! Auto-generated provider boilerplate
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   ./carina-provider-aws/scripts/generate-provider.sh

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult, ResourceSchema, ResourceType};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;

// ===== ResourceType Implementations =====

/// ec2.vpc resource type
pub struct VpcType;

impl ResourceType for VpcType {
    fn name(&self) -> &'static str {
        "ec2.vpc"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.subnet resource type
pub struct SubnetType;

impl ResourceType for SubnetType {
    fn name(&self) -> &'static str {
        "ec2.subnet"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.internet_gateway resource type
pub struct InternetGatewayType;

impl ResourceType for InternetGatewayType {
    fn name(&self) -> &'static str {
        "ec2.internet_gateway"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.route_table resource type
pub struct RouteTableType;

impl ResourceType for RouteTableType {
    fn name(&self) -> &'static str {
        "ec2.route_table"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.route resource type
pub struct RouteType;

impl ResourceType for RouteType {
    fn name(&self) -> &'static str {
        "ec2.route"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.security_group resource type
pub struct SecurityGroupType;

impl ResourceType for SecurityGroupType {
    fn name(&self) -> &'static str {
        "ec2.security_group"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.security_group_ingress resource type
pub struct SecurityGroupIngressRuleType;

impl ResourceType for SecurityGroupIngressRuleType {
    fn name(&self) -> &'static str {
        "ec2.security_group_ingress"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// ec2.security_group_egress resource type
pub struct SecurityGroupEgressRuleType;

impl ResourceType for SecurityGroupEgressRuleType {
    fn name(&self) -> &'static str {
        "ec2.security_group_egress"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// s3.bucket resource type
pub struct S3BucketType;

impl ResourceType for S3BucketType {
    fn name(&self) -> &'static str {
        "s3.bucket"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// sts.caller_identity resource type
pub struct StsCallerIdentityType;

impl ResourceType for StsCallerIdentityType {
    fn name(&self) -> &'static str {
        "sts.caller_identity"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Returns all resource types for the AWS provider.
pub fn resource_types() -> Vec<Box<dyn ResourceType>> {
    vec![
        Box::new(VpcType),
        Box::new(SubnetType),
        Box::new(InternetGatewayType),
        Box::new(RouteTableType),
        Box::new(RouteType),
        Box::new(SecurityGroupType),
        Box::new(SecurityGroupIngressRuleType),
        Box::new(SecurityGroupEgressRuleType),
        Box::new(S3BucketType),
        Box::new(StsCallerIdentityType),
    ]
}

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

    /// Delete s3.bucket (generated)
    pub(crate) async fn delete_s3_bucket(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.s3_client
            .delete_bucket()
            .bucket(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete bucket")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Update ec2.subnet: apply tag changes and read back (generated)
    pub(crate) async fn update_ec2_subnet(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;
        self.read_ec2_subnet(&id, Some(identifier)).await
    }

    /// Update ec2.internet_gateway: apply tag changes and read back (generated)
    pub(crate) async fn update_ec2_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
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
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
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
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
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
        if let Some(v) = obj.carrier_gateway_id() {
            attributes.insert(
                "carrier_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.core_network_arn() {
            attributes.insert("core_network_arn".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.destination_cidr_block() {
            attributes.insert(
                "destination_cidr_block".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.destination_ipv6_cidr_block() {
            attributes.insert(
                "destination_ipv6_cidr_block".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.destination_prefix_list_id() {
            attributes.insert(
                "destination_prefix_list_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.egress_only_internet_gateway_id() {
            attributes.insert(
                "egress_only_internet_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.gateway_id() {
            attributes.insert("gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.instance_id() {
            attributes.insert("instance_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.local_gateway_id() {
            attributes.insert("local_gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.nat_gateway_id() {
            attributes.insert("nat_gateway_id".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = obj.network_interface_id() {
            attributes.insert(
                "network_interface_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.transit_gateway_id() {
            attributes.insert(
                "transit_gateway_id".to_string(),
                Value::String(v.to_string()),
            );
        }
        if let Some(v) = obj.vpc_peering_connection_id() {
            attributes.insert(
                "vpc_peering_connection_id".to_string(),
                Value::String(v.to_string()),
            );
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
