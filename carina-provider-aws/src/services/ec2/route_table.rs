use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;
use crate::helpers::require_string_attr;

impl AwsProvider {
    /// Read an EC2 Route Table
    pub(crate) async fn read_ec2_route_table(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("route-table-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_route_tables()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe route tables")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            let mut attributes = HashMap::new();

            // Auto-generated attribute extraction
            let identifier_value = Self::extract_ec2_route_table_attributes(rt, &mut attributes);

            // Convert routes to list (complex nested structure)
            let mut routes_list = Vec::new();
            for route in rt.routes() {
                let mut route_map = HashMap::new();
                if let Some(dest) = route.destination_cidr_block() {
                    route_map.insert("destination".to_string(), Value::String(dest.to_string()));
                }
                if let Some(gw) = route.gateway_id() {
                    route_map.insert("gateway_id".to_string(), Value::String(gw.to_string()));
                }
                if !route_map.is_empty() {
                    routes_list.push(Value::Map(route_map));
                }
            }
            if !routes_list.is_empty() {
                attributes.insert("routes".to_string(), Value::List(routes_list));
            }

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(rt.tags()) {
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

    /// Create an EC2 Route Table
    pub(crate) async fn create_ec2_route_table(&self, resource: Resource) -> ProviderResult<State> {
        let vpc_id = require_string_attr(&resource, "vpc_id")?;

        // Create Route Table
        let result = self
            .ec2_client
            .create_route_table()
            .vpc_id(&vpc_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to create route table")
                    .with_cause(e)
                    .for_resource(resource.id.clone())
            })?;

        let rt_id = result
            .route_table()
            .and_then(|rt| rt.route_table_id())
            .ok_or_else(|| {
                ProviderError::new("Route Table created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, rt_id, &resource.resolved_attributes(), None)
            .await?;

        // Add routes
        if let Some(Value::List(routes)) = resource.get_attr("routes") {
            for route in routes {
                if let Value::Map(route_map) = route {
                    let destination = route_map.get("destination").and_then(|v| {
                        if let Value::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    });
                    let gateway_id = route_map.get("gateway_id").and_then(|v| {
                        if let Value::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    });

                    if let (Some(dest), Some(gw_id)) = (destination, gateway_id) {
                        self.ec2_client
                            .create_route()
                            .route_table_id(rt_id)
                            .destination_cidr_block(dest)
                            .gateway_id(gw_id)
                            .send()
                            .await
                            .map_err(|e| {
                                ProviderError::new("Failed to create route")
                                    .with_cause(e)
                                    .for_resource(resource.id.clone())
                            })?;
                    }
                }
            }
        }

        // Read back using route table ID (reliable identifier)
        self.read_ec2_route_table(&resource.id, Some(rt_id)).await
    }
}
