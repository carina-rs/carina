use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;
use crate::helpers::{build_tag_specification, require_string_attr};

impl AwsProvider {
    /// Read an EC2 Flow Log
    pub(crate) async fn read_ec2_flow_log(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("flow-log-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_flow_logs()
            .filter(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe flow logs")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(fl) = result.flow_logs().first() {
            let mut attributes = HashMap::new();

            let identifier_value = Self::extract_ec2_flow_log_attributes(fl, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(fl.tags()) {
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

    /// Create an EC2 Flow Log
    pub(crate) async fn create_ec2_flow_log(&self, resource: Resource) -> ProviderResult<State> {
        let resource_id_val = require_string_attr(&resource, "resource_id")?;

        let resource_type_val = match resource.get_attr("resource_type") {
            Some(Value::String(s)) => extract_enum_value(s).to_string(),
            _ => {
                return Err(ProviderError::new("resource_type is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let mut req = self
            .ec2_client
            .create_flow_logs()
            .resource_ids(&resource_id_val)
            .resource_type(aws_sdk_ec2::types::FlowLogsResourceType::from(
                resource_type_val.as_str(),
            ));

        if let Some(Value::String(traffic_type)) = resource.get_attr("traffic_type") {
            use aws_sdk_ec2::types::TrafficType;
            let tt = TrafficType::from(extract_enum_value(traffic_type));
            req = req.traffic_type(tt);
        }

        if let Some(Value::String(log_dest_type)) = resource.get_attr("log_destination_type") {
            use aws_sdk_ec2::types::LogDestinationType;
            let raw = extract_enum_value(log_dest_type);
            // Map DSL snake_case enum values back to API hyphenated format
            let api_value = match raw {
                "cloud_watch_logs" => "cloud-watch-logs",
                "kinesis_data_firehose" => "kinesis-data-firehose",
                other => other,
            };
            let ldt = LogDestinationType::from(api_value);
            req = req.log_destination_type(ldt);
        }

        if let Some(Value::String(log_dest)) = resource.get_attr("log_destination") {
            req = req.log_destination(log_dest);
        }

        if let Some(Value::String(log_group)) = resource.get_attr("log_group_name") {
            req = req.log_group_name(log_group);
        }

        if let Some(Value::String(perm_arn)) = resource.get_attr("deliver_logs_permission_arn") {
            req = req.deliver_logs_permission_arn(perm_arn);
        }

        if let Some(Value::String(log_format)) = resource.get_attr("log_format") {
            req = req.log_format(log_format);
        }

        if let Some(Value::Int(interval)) = resource.get_attr("max_aggregation_interval") {
            req = req.max_aggregation_interval(*interval as i32);
        }

        // Apply tags via TagSpecifications
        if let Some(tag_spec) =
            build_tag_specification(&resource, aws_sdk_ec2::types::ResourceType::VpcFlowLog)
        {
            req = req.tag_specifications(tag_spec);
        }

        // Retry loop for IAM eventual consistency: newly created IAM roles may
        // not be usable immediately by create_flow_logs.
        let mut last_error = String::new();
        let mut result = None;
        for attempt in 0..12 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            let resp = match req.clone().send().await {
                Ok(resp) => resp,
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    last_error = err_str.clone();
                    // Retry on IAM propagation errors (check both Display and Debug output)
                    if err_str.contains("Unable to assume")
                        || err_str.contains("Not authorized")
                        || err_str.contains("Access Denied")
                    {
                        continue;
                    }
                    return Err(ProviderError::new("Failed to create flow logs")
                        .with_cause(e)
                        .for_resource(resource.id.clone()));
                }
            };

            // Check for unsuccessful items
            if let Some(err) = resp.unsuccessful().first() {
                let msg = err
                    .error()
                    .and_then(|e| e.message())
                    .unwrap_or("unknown error");
                let code = err.error().and_then(|e| e.code()).unwrap_or("");
                last_error = format!("{} ({})", msg, code);
                // Retry on IAM propagation errors
                if msg.contains("Unable to assume IAM role")
                    || msg.contains("Not authorized")
                    || msg.contains("Access Denied")
                    || code == "403"
                {
                    continue;
                }
                return Err(
                    ProviderError::new(format!("Failed to create flow log: {}", msg))
                        .for_resource(resource.id.clone()),
                );
            }

            result = Some(resp);
            break;
        }

        let result = result.ok_or_else(|| {
            ProviderError::new(format!(
                "Failed to create flow log after retries: {}",
                last_error
            ))
            .for_resource(resource.id.clone())
        })?;

        let flow_log_id = result.flow_log_ids().first().ok_or_else(|| {
            ProviderError::new("Flow Log created but no ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Read back
        self.read_ec2_flow_log(&resource.id, Some(flow_log_id))
            .await
    }

    /// Update an EC2 Flow Log (tags only - all other attributes are create_only)
    pub(crate) async fn update_ec2_flow_log(
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
        self.read_ec2_flow_log(&id, Some(identifier)).await
    }

    /// Delete an EC2 Flow Log
    pub(crate) async fn delete_ec2_flow_log(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        let result = self
            .ec2_client
            .delete_flow_logs()
            .flow_log_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete flow logs")
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
                ProviderError::new(format!("Failed to delete flow log: {}", msg))
                    .for_resource(id.clone()),
            );
        }

        Ok(())
    }
}
