use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Egress-Only Internet Gateway
    pub(crate) async fn read_ec2_egress_only_internet_gateway(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .ec2_client
            .describe_egress_only_internet_gateways()
            .egress_only_internet_gateway_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe egress-only internet gateways")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(eigw) = result.egress_only_internet_gateways().first() {
            let mut attributes = HashMap::new();

            let identifier_value =
                Self::extract_ec2_egress_only_internet_gateway_attributes(eigw, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(eigw.tags()) {
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

    /// Create an EC2 Egress-Only Internet Gateway
    pub(crate) async fn create_ec2_egress_only_internet_gateway(
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

        let mut req = self
            .ec2_client
            .create_egress_only_internet_gateway()
            .vpc_id(&vpc_id);

        // Apply tags via TagSpecifications
        if let Some(Value::Map(tags)) = resource.attributes.get("tags") {
            use aws_sdk_ec2::types::{Tag, TagSpecification};
            let mut tag_spec = TagSpecification::builder()
                .resource_type(aws_sdk_ec2::types::ResourceType::EgressOnlyInternetGateway);
            for (key, val) in tags {
                if let Value::String(v) = val {
                    tag_spec = tag_spec.tags(Tag::builder().key(key).value(v).build());
                }
            }
            req = req.tag_specifications(tag_spec.build());
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create egress-only internet gateway")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let eigw_id = result
            .egress_only_internet_gateway()
            .and_then(|eigw| eigw.egress_only_internet_gateway_id())
            .ok_or_else(|| {
                ProviderError::new("Egress-Only Internet Gateway created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Read back
        self.read_ec2_egress_only_internet_gateway(&resource.id, Some(eigw_id))
            .await
    }

    /// Update an EC2 Egress-Only Internet Gateway (tags only)
    pub(crate) async fn update_ec2_egress_only_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;
        self.read_ec2_egress_only_internet_gateway(&id, Some(identifier))
            .await
    }

    /// Delete an EC2 Egress-Only Internet Gateway
    pub(crate) async fn delete_ec2_egress_only_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_egress_only_internet_gateway()
            .egress_only_internet_gateway_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete egress-only internet gateway")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }
}
