use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;
use crate::helpers::require_string_attr;

impl AwsProvider {
    /// Read a CloudWatch Logs Log Group
    pub(crate) async fn read_logs_log_group(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .logs_client
            .describe_log_groups()
            .log_group_name_prefix(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe log groups")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        // Find exact match (prefix search can return multiple results)
        if let Some(lg) = result
            .log_groups()
            .iter()
            .find(|lg| lg.log_group_name() == Some(identifier))
        {
            let mut attributes = HashMap::new();

            if let Some(name) = lg.log_group_name() {
                attributes.insert(
                    "log_group_name".to_string(),
                    Value::String(name.to_string()),
                );
            }

            if let Some(arn) = lg.arn() {
                attributes.insert("arn".to_string(), Value::String(arn.to_string()));
            }

            if let Some(days) = lg.retention_in_days() {
                attributes.insert("retention_in_days".to_string(), Value::Int(days as i64));
            }

            if let Some(kms_key) = lg.kms_key_id() {
                attributes.insert("kms_key_id".to_string(), Value::String(kms_key.to_string()));
            }

            if let Some(class) = lg.log_group_class() {
                attributes.insert(
                    "log_group_class".to_string(),
                    Value::String(class.as_str().to_string()),
                );
            }

            // Read tags separately
            #[allow(deprecated)]
            match self
                .logs_client
                .list_tags_log_group()
                .log_group_name(identifier)
                .send()
                .await
            {
                Ok(tags_output) => {
                    if let Some(tags) = tags_output.tags()
                        && !tags.is_empty()
                    {
                        let mut tag_map = HashMap::new();
                        for (key, val) in tags {
                            tag_map.insert(key.to_string(), Value::String(val.to_string()));
                        }
                        attributes.insert("tags".to_string(), Value::Map(tag_map));
                    }
                }
                Err(_) => {
                    // Tags API may fail for some log groups; ignore
                }
            }

            let state = State::existing(id.clone(), attributes);
            Ok(state.with_identifier(identifier.to_string()))
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create a CloudWatch Logs Log Group
    pub(crate) async fn create_logs_log_group(&self, resource: Resource) -> ProviderResult<State> {
        let log_group_name = require_string_attr(&resource, "log_group_name")?;

        let mut req = self
            .logs_client
            .create_log_group()
            .log_group_name(&log_group_name);

        if let Some(Value::String(kms_key)) = resource.get_attr("kms_key_id") {
            req = req.kms_key_id(kms_key);
        }

        if let Some(Value::String(class)) = resource.get_attr("log_group_class") {
            use aws_sdk_cloudwatchlogs::types::LogGroupClass;
            let class_value = extract_enum_value(class);
            req = req.log_group_class(LogGroupClass::from(class_value));
        }

        if let Some(Value::Map(tag_map)) = resource.get_attr("tags") {
            let mut tags = HashMap::new();
            for (key, value) in tag_map {
                if let Value::String(val) = value {
                    tags.insert(key.clone(), val.clone());
                }
            }
            req = req.set_tags(Some(tags));
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to create log group")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        // Set retention if specified
        if let Some(Value::Int(days)) = resource.get_attr("retention_in_days") {
            self.logs_client
                .put_retention_policy()
                .log_group_name(&log_group_name)
                .retention_in_days(*days as i32)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to set retention policy")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;
        }

        self.read_logs_log_group(&resource.id, Some(&log_group_name))
            .await
    }

    /// Update a CloudWatch Logs Log Group
    pub(crate) async fn update_logs_log_group(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        // Update retention
        match to.get_attr("retention_in_days") {
            Some(Value::Int(days)) => {
                self.logs_client
                    .put_retention_policy()
                    .log_group_name(identifier)
                    .retention_in_days(*days as i32)
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new("Failed to set retention policy")
                            .with_cause(e)
                            .for_resource(id.clone())
                    })?;
            }
            None => {
                // If retention was previously set but now removed, delete the policy
                if from.attributes.contains_key("retention_in_days") {
                    self.logs_client
                        .delete_retention_policy()
                        .log_group_name(identifier)
                        .send()
                        .await
                        .map_err(|e| {
                            ProviderError::new("Failed to delete retention policy")
                                .with_cause(e)
                                .for_resource(id.clone())
                        })?;
                }
            }
            _ => {}
        }

        // Update KMS key
        if let Some(Value::String(kms_key)) = to.get_attr("kms_key_id") {
            self.logs_client
                .associate_kms_key()
                .log_group_name(identifier)
                .kms_key_id(kms_key)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to associate KMS key")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        } else if from.attributes.contains_key("kms_key_id") {
            self.logs_client
                .disassociate_kms_key()
                .log_group_name(identifier)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to disassociate KMS key")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        // Update tags
        self.apply_logs_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;

        self.read_logs_log_group(&id, Some(identifier)).await
    }

    /// Delete a CloudWatch Logs Log Group
    pub(crate) async fn delete_logs_log_group(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.logs_client
            .delete_log_group()
            .log_group_name(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete log group")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Apply CloudWatch Logs tags
    async fn apply_logs_tags(
        &self,
        id: &ResourceId,
        log_group_name: &str,
        desired: &HashMap<String, Value>,
        current: Option<&HashMap<String, Value>>,
    ) -> ProviderResult<()> {
        let desired_tags = match desired.get("tags") {
            Some(Value::Map(m)) => m.clone(),
            _ => HashMap::new(),
        };
        let current_tags = match current.and_then(|c| c.get("tags")) {
            Some(Value::Map(m)) => m.clone(),
            _ => HashMap::new(),
        };

        // Tags to remove
        let keys_to_remove: Vec<String> = current_tags
            .keys()
            .filter(|k| !desired_tags.contains_key(*k))
            .cloned()
            .collect();

        if !keys_to_remove.is_empty() {
            #[allow(deprecated)]
            let mut req = self
                .logs_client
                .untag_log_group()
                .log_group_name(log_group_name);
            for key in &keys_to_remove {
                req = req.tags(key);
            }
            req.send().await.map_err(|e| {
                ProviderError::new("Failed to untag log group")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        // Tags to add/update
        let mut tags_to_add = HashMap::new();
        for (key, value) in &desired_tags {
            if let Value::String(val) = value {
                let should_add = match current_tags.get(key) {
                    Some(Value::String(current_val)) => current_val != val,
                    _ => true,
                };
                if should_add {
                    tags_to_add.insert(key.clone(), val.clone());
                }
            }
        }

        if !tags_to_add.is_empty() {
            #[allow(deprecated)]
            self.logs_client
                .tag_log_group()
                .log_group_name(log_group_name)
                .set_tags(Some(tags_to_add))
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to tag log group")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        Ok(())
    }
}
