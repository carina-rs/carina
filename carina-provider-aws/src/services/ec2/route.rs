use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Route (routes are identified by route_table_id + destination)
    pub(crate) async fn read_ec2_route(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> ProviderResult<State> {
        // Routes are identified by route_table_id + destination_cidr_block
        // For read, we return not_found since we can't look up by identifier alone
        Ok(State::not_found(id.clone()))
    }

    /// Read an EC2 Route by route_table_id and destination_cidr_block
    pub async fn read_ec2_route_by_key(
        &self,
        name: &str,
        route_table_id: &str,
        destination_cidr_block: &str,
    ) -> ProviderResult<State> {
        let id = ResourceId::with_provider("aws", "ec2.route", name);

        // Describe the route table to get its routes
        let result = self
            .ec2_client
            .describe_route_tables()
            .route_table_ids(route_table_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe route table: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            // Find the route matching destination_cidr_block
            for route in rt.routes() {
                if route.destination_cidr_block() == Some(destination_cidr_block) {
                    let mut attributes = HashMap::new();

                    // Auto-generated attribute extraction
                    Self::extract_ec2_route_attributes(route, &mut attributes);

                    // route_table_id is not in the Route struct, add from parameter
                    attributes.insert(
                        "route_table_id".to_string(),
                        Value::String(route_table_id.to_string()),
                    );

                    // Route identifier is route_table_id|destination_cidr_block
                    let identifier = format!("{}|{}", route_table_id, destination_cidr_block);
                    return Ok(State::existing(id, attributes).with_identifier(identifier));
                }
            }
        }

        Ok(State::not_found(id))
    }

    /// Create an EC2 Route
    pub(crate) async fn create_ec2_route(&self, resource: Resource) -> ProviderResult<State> {
        let route_table_id = match resource.attributes.get("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("route_table_id is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let destination_cidr = match resource.attributes.get("destination_cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("destination_cidr_block is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let mut req = self
            .ec2_client
            .create_route()
            .route_table_id(&route_table_id)
            .destination_cidr_block(&destination_cidr);

        // Add gateway_id if specified
        if let Some(Value::String(gw_id)) = resource.attributes.get("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = resource.attributes.get("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create route: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(resource.id, resource.attributes).with_identifier(identifier))
    }

    /// Update an EC2 Route (replace the route)
    pub(crate) async fn update_ec2_route(
        &self,
        id: ResourceId,
        _identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        let route_table_id = match to.attributes.get("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("route_table_id is required").for_resource(id.clone())
                );
            }
        };

        let destination_cidr = match to.attributes.get("destination_cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("destination_cidr_block is required")
                    .for_resource(id.clone()));
            }
        };

        let mut req = self
            .ec2_client
            .replace_route()
            .route_table_id(&route_table_id)
            .destination_cidr_block(&destination_cidr);

        // Add gateway_id if specified
        if let Some(Value::String(gw_id)) = to.attributes.get("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = to.attributes.get("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to update route: {:?}", e)).for_resource(id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(id, to.attributes.clone()).with_identifier(identifier))
    }
}
