use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use aws_sdk_ec2::types::NatGatewayState;

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 NAT Gateway
    pub(crate) async fn read_ec2_nat_gateway(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("nat-gateway-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_nat_gateways()
            .filter(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe NAT gateways")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(ngw) = result.nat_gateways().first() {
            // Skip deleted NAT gateways
            if ngw.state() == Some(&NatGatewayState::Deleted) {
                return Ok(State::not_found(id.clone()));
            }

            let mut attributes = HashMap::new();

            // Extract attributes
            let identifier_value = Self::extract_ec2_nat_gateway_attributes(ngw, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(ngw.tags()) {
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

    /// Create an EC2 NAT Gateway
    pub(crate) async fn create_ec2_nat_gateway(&self, resource: Resource) -> ProviderResult<State> {
        let subnet_id = match resource.get_attr("subnet_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("subnet_id is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self.ec2_client.create_nat_gateway().subnet_id(&subnet_id);

        if let Some(Value::String(alloc_id)) = resource.get_attr("allocation_id") {
            req = req.allocation_id(alloc_id);
        }

        if let Some(Value::String(conn_type)) = resource.get_attr("connectivity_type") {
            use aws_sdk_ec2::types::ConnectivityType;
            let ct = ConnectivityType::from(extract_enum_value(conn_type));
            req = req.connectivity_type(ct);
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create NAT gateway")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let ngw_id = result
            .nat_gateway()
            .and_then(|ngw| ngw.nat_gateway_id())
            .ok_or_else(|| {
                ProviderError::new("NAT Gateway created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, ngw_id, &resource.resolved_attributes(), None)
            .await?;

        // Wait for NAT gateway to become available
        self.wait_for_nat_gateway_available(&resource.id, ngw_id)
            .await?;

        // Read back using NAT gateway ID
        self.read_ec2_nat_gateway(&resource.id, Some(ngw_id)).await
    }

    /// Update an EC2 NAT Gateway (tags only)
    pub(crate) async fn update_ec2_nat_gateway(
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
        self.read_ec2_nat_gateway(&id, Some(identifier)).await
    }

    /// Delete an EC2 NAT Gateway
    pub(crate) async fn delete_ec2_nat_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_nat_gateway()
            .nat_gateway_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete NAT gateway")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        // Wait for NAT gateway to be deleted
        self.wait_for_nat_gateway_deleted(&id, identifier).await?;

        Ok(())
    }

    /// Wait for a NAT gateway to reach the "available" state
    async fn wait_for_nat_gateway_available(
        &self,
        id: &ResourceId,
        nat_gateway_id: &str,
    ) -> ProviderResult<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        for _ in 0..60 {
            let result = self
                .ec2_client
                .describe_nat_gateways()
                .nat_gateway_ids(nat_gateway_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to describe NAT gateway")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;

            if let Some(ngw) = result.nat_gateways().first()
                && let Some(state) = ngw.state()
            {
                if *state == NatGatewayState::Available {
                    return Ok(());
                }
                if *state == NatGatewayState::Failed {
                    return Err(
                        ProviderError::new("NAT gateway creation failed").for_resource(id.clone())
                    );
                }
                // "pending" - keep waiting
            }

            sleep(Duration::from_secs(5)).await;
        }

        Err(
            ProviderError::new("Timeout waiting for NAT gateway to become available")
                .for_resource(id.clone()),
        )
    }

    /// Wait for a NAT gateway to be deleted
    async fn wait_for_nat_gateway_deleted(
        &self,
        id: &ResourceId,
        nat_gateway_id: &str,
    ) -> ProviderResult<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        for _ in 0..60 {
            let result = self
                .ec2_client
                .describe_nat_gateways()
                .nat_gateway_ids(nat_gateway_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to describe NAT gateway")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;

            if let Some(ngw) = result.nat_gateways().first() {
                if ngw.state() == Some(&NatGatewayState::Deleted) {
                    return Ok(());
                }
            } else {
                return Ok(());
            }

            sleep(Duration::from_secs(5)).await;
        }

        Err(
            ProviderError::new("Timeout waiting for NAT gateway to be deleted")
                .for_resource(id.clone()),
        )
    }
}
