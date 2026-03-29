use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value_with_values;

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 VPN Gateway
    pub(crate) async fn read_ec2_vpn_gateway(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("vpn-gateway-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_vpn_gateways()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe VPN gateways")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(vgw) = result.vpn_gateways().first() {
            // Skip deleted VPN gateways
            if vgw.state().map(|s| s.as_str()) == Some("deleted") {
                return Ok(State::not_found(id.clone()));
            }

            let mut attributes = HashMap::new();

            let identifier_value = Self::extract_ec2_vpn_gateway_attributes(vgw, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(vgw.tags()) {
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

    /// Create an EC2 VPN Gateway
    pub(crate) async fn create_ec2_vpn_gateway(&self, resource: Resource) -> ProviderResult<State> {
        let gw_type = match resource.get_attr("type") {
            Some(Value::String(s)) => extract_enum_value_with_values(s, &["ipsec.1"]).to_string(),
            _ => {
                return Err(
                    ProviderError::new("type is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self
            .ec2_client
            .create_vpn_gateway()
            .r#type(aws_sdk_ec2::types::GatewayType::from(gw_type.as_str()));

        if let Some(Value::Int(asn)) = resource.get_attr("amazon_side_asn") {
            req = req.amazon_side_asn(*asn);
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create VPN gateway")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let vgw_id = result
            .vpn_gateway()
            .and_then(|vgw| vgw.vpn_gateway_id())
            .ok_or_else(|| {
                ProviderError::new("VPN Gateway created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, vgw_id, &resource.resolved_attributes(), None)
            .await?;

        // Read back
        self.read_ec2_vpn_gateway(&resource.id, Some(vgw_id)).await
    }

    /// Update an EC2 VPN Gateway (tags only)
    pub(crate) async fn update_ec2_vpn_gateway(
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
        self.read_ec2_vpn_gateway(&id, Some(identifier)).await
    }

    /// Delete an EC2 VPN Gateway
    pub(crate) async fn delete_ec2_vpn_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_vpn_gateway()
            .vpn_gateway_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete VPN gateway")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }
}
