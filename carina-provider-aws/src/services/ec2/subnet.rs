use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::convert_enum_value;

use crate::AwsProvider;
use crate::helpers::require_string_attr;
use aws_sdk_ec2::types::{AttributeBooleanValue, HostnameType};

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
        let cidr_block = require_string_attr(&resource, "cidr_block")?;
        let vpc_id = require_string_attr(&resource, "vpc_id")?;

        let mut req = self
            .ec2_client
            .create_subnet()
            .vpc_id(&vpc_id)
            .cidr_block(&cidr_block);

        if let Some(Value::String(az)) = resource.get_attr("availability_zone") {
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
        let attrs = resource.resolved_attributes();
        self.apply_ec2_tags(&resource.id, subnet_id, &attrs, None)
            .await?;

        // Apply subnet attributes that require ModifySubnetAttribute
        self.modify_subnet_attributes(&resource.id, subnet_id, &attrs)
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
        let attrs = to.resolved_attributes();
        self.modify_subnet_attributes(&id, identifier, &attrs)
            .await?;

        // Update tags
        self.apply_ec2_tags(&id, identifier, &attrs, Some(&from.attributes))
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

        // The ModifySubnetAttribute API only allows modifying one attribute at a time.
        // Each private_dns_name_options_on_launch field must be a separate API call.
        if let Some(Value::Map(fields)) = attributes.get("private_dns_name_options_on_launch") {
            if let Some(Value::String(ht)) = fields.get("hostname_type") {
                let hostname_val = convert_enum_value(ht);
                self.ec2_client
                    .modify_subnet_attribute()
                    .subnet_id(subnet_id)
                    .private_dns_hostname_type_on_launch(HostnameType::from(hostname_val.as_str()))
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new(
                            "Failed to set private_dns_name_options_on_launch.hostname_type",
                        )
                        .with_cause(e)
                        .for_resource(id.clone())
                    })?;
            }
            if let Some(Value::Bool(v)) = fields.get("enable_resource_name_dns_a_record") {
                self.ec2_client
                    .modify_subnet_attribute()
                    .subnet_id(subnet_id)
                    .enable_resource_name_dns_a_record_on_launch(
                        AttributeBooleanValue::builder().value(*v).build(),
                    )
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new(
                            "Failed to set private_dns_name_options_on_launch.enable_resource_name_dns_a_record",
                        )
                        .with_cause(e)
                        .for_resource(id.clone())
                    })?;
            }
            if let Some(Value::Bool(v)) = fields.get("enable_resource_name_dns_aaaa_record") {
                self.ec2_client
                    .modify_subnet_attribute()
                    .subnet_id(subnet_id)
                    .enable_resource_name_dns_aaaa_record_on_launch(
                        AttributeBooleanValue::builder().value(*v).build(),
                    )
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new(
                            "Failed to set private_dns_name_options_on_launch.enable_resource_name_dns_aaaa_record",
                        )
                        .with_cause(e)
                        .for_resource(id.clone())
                    })?;
            }
        }

        Ok(())
    }
}
