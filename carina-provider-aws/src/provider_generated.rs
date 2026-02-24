//! Auto-generated provider boilerplate
//!
//! DO NOT EDIT MANUALLY - regenerate with:
//!   ./carina-provider-aws/scripts/generate-provider.sh

use carina_core::provider::{ProviderError, ProviderResult, ResourceSchema, ResourceType};
use carina_core::resource::{Resource, ResourceId, State};

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
                ProviderError::new(format!("Failed to delete vpc: {:?}", e))
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
                ProviderError::new(format!("Failed to delete subnet: {:?}", e))
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
                ProviderError::new(format!("Failed to delete route table: {:?}", e))
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
                ProviderError::new(format!("Failed to delete security group: {:?}", e))
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
                ProviderError::new(format!("Failed to delete bucket: {:?}", e))
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Update ec2.subnet (no-op, just read back current state) (generated)
    pub(crate) async fn update_ec2_subnet(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        self.read_ec2_subnet(&id, Some(identifier)).await
    }

    /// Update ec2.internet_gateway (no-op, just read back current state) (generated)
    pub(crate) async fn update_ec2_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        self.read_ec2_internet_gateway(&id, Some(identifier)).await
    }

    /// Update ec2.route_table (no-op, just read back current state) (generated)
    pub(crate) async fn update_ec2_route_table(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        self.read_ec2_route_table(&id, Some(identifier)).await
    }

    /// Update ec2.security_group (no-op, just read back current state) (generated)
    pub(crate) async fn update_ec2_security_group(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        self.read_ec2_security_group(&id, Some(identifier)).await
    }
}
