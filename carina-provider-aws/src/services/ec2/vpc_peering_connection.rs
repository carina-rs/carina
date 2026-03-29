use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 VPC Peering Connection
    pub(crate) async fn read_ec2_vpc_peering_connection(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .ec2_client
            .describe_vpc_peering_connections()
            .vpc_peering_connection_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe VPC peering connections")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(pcx) = result.vpc_peering_connections().first() {
            // Skip deleted/failed peering connections
            let status_code = pcx
                .status()
                .and_then(|s| s.code())
                .map(|c| c.as_str().to_string());
            if matches!(
                status_code.as_deref(),
                Some("deleted") | Some("failed") | Some("rejected") | Some("expired")
            ) {
                return Ok(State::not_found(id.clone()));
            }

            let mut attributes = HashMap::new();

            let identifier_value =
                Self::extract_ec2_vpc_peering_connection_attributes(pcx, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(pcx.tags()) {
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

    /// Create an EC2 VPC Peering Connection
    pub(crate) async fn create_ec2_vpc_peering_connection(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("vpc_id is required").for_resource(resource.id.clone())
                );
            }
        };

        let peer_vpc_id = match resource.attributes.get("peer_vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("peer_vpc_id is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self
            .ec2_client
            .create_vpc_peering_connection()
            .vpc_id(&vpc_id)
            .peer_vpc_id(&peer_vpc_id);

        if let Some(Value::String(owner_id)) = resource.attributes.get("peer_owner_id") {
            req = req.peer_owner_id(owner_id);
        }

        if let Some(Value::String(region)) = resource.attributes.get("peer_region") {
            req = req.peer_region(region);
        }

        // Apply tags via TagSpecifications
        if let Some(Value::Map(tags)) = resource.attributes.get("tags") {
            use aws_sdk_ec2::types::{Tag, TagSpecification};
            let mut tag_spec = TagSpecification::builder()
                .resource_type(aws_sdk_ec2::types::ResourceType::VpcPeeringConnection);
            for (key, val) in tags {
                if let Value::String(v) = val {
                    tag_spec = tag_spec.tags(Tag::builder().key(key).value(v).build());
                }
            }
            req = req.tag_specifications(tag_spec.build());
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create VPC peering connection")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let pcx_id = result
            .vpc_peering_connection()
            .and_then(|pcx| pcx.vpc_peering_connection_id())
            .ok_or_else(|| {
                ProviderError::new("VPC Peering Connection created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Read back
        self.read_ec2_vpc_peering_connection(&resource.id, Some(pcx_id))
            .await
    }

    /// Update an EC2 VPC Peering Connection (tags only)
    pub(crate) async fn update_ec2_vpc_peering_connection(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;
        self.read_ec2_vpc_peering_connection(&id, Some(identifier))
            .await
    }

    /// Delete an EC2 VPC Peering Connection
    pub(crate) async fn delete_ec2_vpc_peering_connection(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_vpc_peering_connection()
            .vpc_peering_connection_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete VPC peering connection")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }
}
