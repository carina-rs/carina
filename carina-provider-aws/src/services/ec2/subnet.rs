use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::convert_enum_value;

use crate::AwsProvider;
use aws_sdk_ec2::types::AttributeBooleanValue;

impl AwsProvider {
    /// Read an EC2 Subnet
    pub(crate) async fn read_ec2_subnet(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("subnet-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_subnets()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe subnets")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(subnet) = result.subnets().first() {
            let mut attributes = HashMap::new();

            // Auto-generated attribute extraction
            let identifier_value = Self::extract_ec2_subnet_attributes(subnet, &mut attributes);

            // Override availability_zone with DSL format
            if let Some(az) = subnet.availability_zone() {
                let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
                attributes.insert("availability_zone".to_string(), Value::String(az_dsl));
            }

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(subnet.tags()) {
                attributes.insert("tags".to_string(), tags_value);
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

    /// Create an EC2 Subnet
    pub(crate) async fn create_ec2_subnet(&self, resource: Resource) -> ProviderResult<State> {
        let cidr_block = match resource.attributes.get("cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("CIDR block is required").for_resource(resource.id.clone())
                );
            }
        };

        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC ID is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self
            .ec2_client
            .create_subnet()
            .vpc_id(&vpc_id)
            .cidr_block(&cidr_block);

        if let Some(Value::String(az)) = resource.attributes.get("availability_zone") {
            req = req.availability_zone(convert_enum_value(az));
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create subnet")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let subnet_id = result.subnet().and_then(|s| s.subnet_id()).ok_or_else(|| {
            ProviderError::new("Subnet created but no ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, subnet_id, &resource.attributes, None)
            .await?;

        // Apply subnet attributes that require ModifySubnetAttribute
        self.modify_subnet_attributes(&resource.id, subnet_id, &resource.attributes)
            .await?;

        // Read back using subnet ID (reliable identifier)
        self.read_ec2_subnet(&resource.id, Some(subnet_id)).await
    }

    /// Update an EC2 Subnet
    pub(crate) async fn update_ec2_subnet(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        // Apply subnet attributes that require ModifySubnetAttribute
        self.modify_subnet_attributes(&id, identifier, &to.attributes)
            .await?;

        // Update tags
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;

        self.read_ec2_subnet(&id, Some(identifier)).await
    }

    /// Apply boolean subnet attributes via ModifySubnetAttribute API.
    /// Used by both create (post-creation) and update paths.
    async fn modify_subnet_attributes(
        &self,
        id: &ResourceId,
        subnet_id: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if let Some(Value::Bool(enabled)) = attributes.get("map_public_ip_on_launch") {
            self.ec2_client
                .modify_subnet_attribute()
                .subnet_id(subnet_id)
                .map_public_ip_on_launch(AttributeBooleanValue::builder().value(*enabled).build())
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set map_public_ip_on_launch")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        if let Some(Value::Bool(enabled)) = attributes.get("assign_ipv6_address_on_creation") {
            self.ec2_client
                .modify_subnet_attribute()
                .subnet_id(subnet_id)
                .assign_ipv6_address_on_creation(
                    AttributeBooleanValue::builder().value(*enabled).build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set assign_ipv6_address_on_creation")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        if let Some(Value::Bool(enabled)) = attributes.get("enable_dns64") {
            self.ec2_client
                .modify_subnet_attribute()
                .subnet_id(subnet_id)
                .enable_dns64(AttributeBooleanValue::builder().value(*enabled).build())
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set enable_dns64")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        Ok(())
    }
}
