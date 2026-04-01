use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;
use crate::helpers::{require_string_attr, retry_aws_operation};

impl AwsProvider {
    /// Read an EC2 VPC Endpoint
    pub(crate) async fn read_ec2_vpc_endpoint(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .ec2_client
            .describe_vpc_endpoints()
            .vpc_endpoint_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe VPC endpoints")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(endpoint) = result.vpc_endpoints().first() {
            // Skip deleted endpoints
            if endpoint.state().map(|s| s.as_str()) == Some("deleted") {
                return Ok(State::not_found(id.clone()));
            }

            let mut attributes = HashMap::new();

            let identifier_value =
                Self::extract_ec2_vpc_endpoint_attributes(endpoint, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(endpoint.tags()) {
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

    /// Create an EC2 VPC Endpoint
    pub(crate) async fn create_ec2_vpc_endpoint(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let vpc_id = require_string_attr(&resource, "vpc_id")?;
        let service_name = require_string_attr(&resource, "service_name")?;

        let mut req = self
            .ec2_client
            .create_vpc_endpoint()
            .vpc_id(&vpc_id)
            .service_name(&service_name);

        if let Some(Value::String(ep_type)) = resource.get_attr("vpc_endpoint_type") {
            use aws_sdk_ec2::types::VpcEndpointType;
            let et = VpcEndpointType::from(extract_enum_value(ep_type));
            req = req.vpc_endpoint_type(et);
        }

        if let Some(Value::List(ids)) = resource.get_attr("route_table_ids") {
            for id_val in ids {
                if let Value::String(s) = id_val {
                    req = req.route_table_ids(s);
                }
            }
        }

        if let Some(Value::List(ids)) = resource.get_attr("subnet_ids") {
            for id_val in ids {
                if let Value::String(s) = id_val {
                    req = req.subnet_ids(s);
                }
            }
        }

        if let Some(Value::List(ids)) = resource.get_attr("security_group_ids") {
            for id_val in ids {
                if let Value::String(s) = id_val {
                    req = req.security_group_ids(s);
                }
            }
        }

        if let Some(Value::Bool(v)) = resource.get_attr("private_dns_enabled") {
            req = req.private_dns_enabled(*v);
        }

        if let Some(Value::String(policy)) = resource.get_attr("policy_document") {
            req = req.policy_document(policy);
        } else if let Some(Value::Map(map)) = resource.get_attr("policy_document") {
            // Convert Value::Map to JSON string for the API
            let json_str =
                crate::services::iam::role::value_to_iam_policy_json(&Value::Map(map.clone()))
                    .map_err(|e| {
                        ProviderError::new(format!("Failed to serialize policy_document: {}", e))
                            .for_resource(resource.id.clone())
                    })?;
            req = req.policy_document(&json_str);
        }

        let rid = resource.id.clone();
        let result = retry_aws_operation("create VPC endpoint", 5, 5, || {
            let req = req.clone();
            let rid = rid.clone();
            async move {
                req.send().await.map_err(|e| {
                    ProviderError::new("Failed to create VPC endpoint")
                        .with_cause(e)
                        .for_resource(rid)
                })
            }
        })
        .await?;

        let endpoint_id = result
            .vpc_endpoint()
            .and_then(|ep| ep.vpc_endpoint_id())
            .ok_or_else(|| {
                ProviderError::new("VPC Endpoint created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Apply tags
        self.apply_ec2_tags(
            &resource.id,
            endpoint_id,
            &resource.resolved_attributes(),
            None,
        )
        .await?;

        // Read back
        self.read_ec2_vpc_endpoint(&resource.id, Some(endpoint_id))
            .await
    }

    /// Update an EC2 VPC Endpoint
    pub(crate) async fn update_ec2_vpc_endpoint(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        let mut req = self
            .ec2_client
            .modify_vpc_endpoint()
            .vpc_endpoint_id(identifier);

        let mut has_modifications = false;

        // Update route_table_ids
        if let Some(Value::List(new_ids)) = to.get_attr("route_table_ids") {
            let old_ids: Vec<String> =
                if let Some(Value::List(old)) = from.attributes.get("route_table_ids") {
                    old.iter()
                        .filter_map(|v| {
                            if let Value::String(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    vec![]
                };
            let new_id_strs: Vec<String> = new_ids
                .iter()
                .filter_map(|v| {
                    if let Value::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();

            for id_val in &new_id_strs {
                if !old_ids.contains(id_val) {
                    req = req.add_route_table_ids(id_val);
                    has_modifications = true;
                }
            }
            for id_val in &old_ids {
                if !new_id_strs.contains(id_val) {
                    req = req.remove_route_table_ids(id_val);
                    has_modifications = true;
                }
            }
        }

        // Update subnet_ids
        if let Some(Value::List(new_ids)) = to.get_attr("subnet_ids") {
            let old_ids: Vec<String> =
                if let Some(Value::List(old)) = from.attributes.get("subnet_ids") {
                    old.iter()
                        .filter_map(|v| {
                            if let Value::String(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    vec![]
                };
            let new_id_strs: Vec<String> = new_ids
                .iter()
                .filter_map(|v| {
                    if let Value::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();

            for id_val in &new_id_strs {
                if !old_ids.contains(id_val) {
                    req = req.add_subnet_ids(id_val);
                    has_modifications = true;
                }
            }
            for id_val in &old_ids {
                if !new_id_strs.contains(id_val) {
                    req = req.remove_subnet_ids(id_val);
                    has_modifications = true;
                }
            }
        }

        // Update security_group_ids
        if let Some(Value::List(new_ids)) = to.get_attr("security_group_ids") {
            let old_ids: Vec<String> =
                if let Some(Value::List(old)) = from.attributes.get("security_group_ids") {
                    old.iter()
                        .filter_map(|v| {
                            if let Value::String(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    vec![]
                };
            let new_id_strs: Vec<String> = new_ids
                .iter()
                .filter_map(|v| {
                    if let Value::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();

            for id_val in &new_id_strs {
                if !old_ids.contains(id_val) {
                    req = req.add_security_group_ids(id_val);
                    has_modifications = true;
                }
            }
            for id_val in &old_ids {
                if !new_id_strs.contains(id_val) {
                    req = req.remove_security_group_ids(id_val);
                    has_modifications = true;
                }
            }
        }

        // Update private_dns_enabled
        if let Some(Value::Bool(v)) = to.get_attr("private_dns_enabled") {
            req = req.private_dns_enabled(*v);
            has_modifications = true;
        }

        // Update policy_document
        if let Some(Value::String(policy)) = to.get_attr("policy_document") {
            req = req.policy_document(policy);
            has_modifications = true;
        } else if let Some(Value::Map(map)) = to.get_attr("policy_document") {
            let json_str =
                crate::services::iam::role::value_to_iam_policy_json(&Value::Map(map.clone()))
                    .map_err(|e| {
                        ProviderError::new(format!("Failed to serialize policy_document: {}", e))
                            .for_resource(id.clone())
                    })?;
            req = req.policy_document(&json_str);
            has_modifications = true;
        }

        if has_modifications {
            req.send().await.map_err(|e| {
                ProviderError::new("Failed to modify VPC endpoint")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        // Apply tags
        self.apply_ec2_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;

        self.read_ec2_vpc_endpoint(&id, Some(identifier)).await
    }

    /// Delete an EC2 VPC Endpoint
    pub(crate) async fn delete_ec2_vpc_endpoint(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        let result = self
            .ec2_client
            .delete_vpc_endpoints()
            .vpc_endpoint_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete VPC endpoint")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        // Check for unsuccessful items
        if let Some(err) = result.unsuccessful().first() {
            let msg = err
                .error()
                .and_then(|e| e.message())
                .unwrap_or("unknown error");
            return Err(
                ProviderError::new(format!("Failed to delete VPC endpoint: {}", msg))
                    .for_resource(id.clone()),
            );
        }

        Ok(())
    }
}
