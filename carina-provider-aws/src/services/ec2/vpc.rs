use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;
use crate::helpers::{require_string_attr, retry_aws_operation};

impl AwsProvider {
    /// Read an EC2 VPC
    pub(crate) async fn read_ec2_vpc(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder().name("vpc-id").values(identifier).build();

        let result = self
            .ec2_client
            .describe_vpcs()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe VPCs")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(vpc) = result.vpcs().first() {
            let mut attributes = HashMap::new();

            // Auto-generated attribute extraction
            let identifier_value = Self::extract_ec2_vpc_attributes(vpc, &mut attributes);

            // Extract user-defined tags (excluding Name)
            if let Some(tags_value) = Self::ec2_tags_to_value(vpc.tags()) {
                attributes.insert("tags".to_string(), tags_value);
            }

            // Get VPC attributes for DNS settings (not in Vpc struct)
            if let Some(vpc_id) = vpc.vpc_id() {
                if let Ok(dns_support) = self
                    .ec2_client
                    .describe_vpc_attribute()
                    .vpc_id(vpc_id)
                    .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsSupport)
                    .send()
                    .await
                    && let Some(attr) = dns_support.enable_dns_support()
                {
                    attributes.insert(
                        "enable_dns_support".to_string(),
                        Value::Bool(attr.value.unwrap_or(false)),
                    );
                }

                if let Ok(dns_hostnames) = self
                    .ec2_client
                    .describe_vpc_attribute()
                    .vpc_id(vpc_id)
                    .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsHostnames)
                    .send()
                    .await
                    && let Some(attr) = dns_hostnames.enable_dns_hostnames()
                {
                    attributes.insert(
                        "enable_dns_hostnames".to_string(),
                        Value::Bool(attr.value.unwrap_or(false)),
                    );
                }
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(id_val) = identifier_value {
                state.with_identifier(id_val)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 VPC
    pub(crate) async fn create_ec2_vpc(&self, resource: Resource) -> ProviderResult<State> {
        let cidr_block = require_string_attr(&resource, "cidr_block")?;

        // Create VPC with optional instance_tenancy
        let mut create_vpc_builder = self.ec2_client.create_vpc().cidr_block(&cidr_block);

        // Handle instance_tenancy if specified
        if let Some(Value::String(tenancy)) = resource.get_attr("instance_tenancy") {
            // Convert DSL format (aws.vpc.InstanceTenancy.dedicated) to API value (dedicated)
            let tenancy_value = extract_enum_value(tenancy);

            let tenancy_enum = match tenancy_value {
                "dedicated" => aws_sdk_ec2::types::Tenancy::Dedicated,
                "host" => aws_sdk_ec2::types::Tenancy::Host,
                _ => aws_sdk_ec2::types::Tenancy::Default,
            };
            create_vpc_builder = create_vpc_builder.instance_tenancy(tenancy_enum);
        }

        let rid = resource.id.clone();
        let result = retry_aws_operation("create VPC", 5, 5, || {
            let builder = create_vpc_builder.clone();
            let rid = rid.clone();
            async move {
                builder.send().await.map_err(|e| {
                    ProviderError::new("Failed to create VPC")
                        .with_cause(e)
                        .for_resource(rid)
                })
            }
        })
        .await?;

        let vpc_id = result.vpc().and_then(|v| v.vpc_id()).ok_or_else(|| {
            ProviderError::new("VPC created but no ID returned").for_resource(resource.id.clone())
        })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, vpc_id, &resource.resolved_attributes(), None)
            .await?;

        // Configure DNS support
        if let Some(Value::Bool(enabled)) = resource.get_attr("enable_dns_support") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(vpc_id)
                .enable_dns_support(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set DNS support")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;
        }

        // Configure DNS hostnames
        if let Some(Value::Bool(enabled)) = resource.get_attr("enable_dns_hostnames") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(vpc_id)
                .enable_dns_hostnames(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set DNS hostnames")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using VPC ID (reliable identifier)
        self.read_ec2_vpc(&resource.id, Some(vpc_id)).await
    }

    /// Update an EC2 VPC
    pub(crate) async fn update_ec2_vpc(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        // identifier is the VPC ID (e.g., vpc-12345678)
        let vpc_id = identifier.to_string();

        // Update DNS support
        if let Some(Value::Bool(enabled)) = to.get_attr("enable_dns_support") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(&vpc_id)
                .enable_dns_support(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to update DNS support")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        // Update DNS hostnames
        if let Some(Value::Bool(enabled)) = to.get_attr("enable_dns_hostnames") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(&vpc_id)
                .enable_dns_hostnames(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to update DNS hostnames")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        // Update tags
        self.apply_ec2_tags(
            &id,
            &vpc_id,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;

        self.read_ec2_vpc(&id, Some(identifier)).await
    }
}
