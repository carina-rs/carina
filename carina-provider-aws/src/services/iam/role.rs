use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an IAM Role
    pub(crate) async fn read_iam_role(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .iam_client
            .get_role()
            .role_name(identifier)
            .send()
            .await;

        match result {
            Ok(output) => {
                if let Some(role) = output.role() {
                    let mut attributes = HashMap::new();

                    let identifier_value = Self::extract_iam_role_attributes(role, &mut attributes);

                    // Extract tags
                    let tags = role.tags();
                    if !tags.is_empty() {
                        let mut tag_map = HashMap::new();
                        for tag in tags {
                            let key = tag.key();
                            let val = tag.value();
                            tag_map.insert(key.to_string(), Value::String(val.to_string()));
                        }
                        if !tag_map.is_empty() {
                            attributes.insert("tags".to_string(), Value::Map(tag_map));
                        }
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
            Err(e) => {
                // Check if it's a NoSuchEntity error
                if let Some(service_err) = e.as_service_error()
                    && service_err.is_no_such_entity_exception()
                {
                    return Ok(State::not_found(id.clone()));
                }
                Err(ProviderError::new("Failed to get IAM role")
                    .with_cause(e)
                    .for_resource(id.clone()))
            }
        }
    }

    /// Create an IAM Role
    pub(crate) async fn create_iam_role(&self, resource: Resource) -> ProviderResult<State> {
        let role_name = match resource.attributes.get("role_name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("role_name is required").for_resource(resource.id.clone())
                );
            }
        };

        let assume_role_policy_document =
            match resource.attributes.get("assume_role_policy_document") {
                Some(Value::String(s)) => s.clone(),
                _ => {
                    return Err(
                        ProviderError::new("assume_role_policy_document is required")
                            .for_resource(resource.id.clone()),
                    );
                }
            };

        let mut req = self
            .iam_client
            .create_role()
            .role_name(&role_name)
            .assume_role_policy_document(&assume_role_policy_document);

        if let Some(Value::String(desc)) = resource.attributes.get("description") {
            req = req.description(desc);
        }

        if let Some(Value::String(path)) = resource.attributes.get("path") {
            req = req.path(path);
        }

        if let Some(Value::Int(duration)) = resource.attributes.get("max_session_duration") {
            req = req.max_session_duration(*duration as i32);
        }

        // Apply tags at creation time
        if let Some(Value::Map(tag_map)) = resource.attributes.get("tags") {
            for (key, value) in tag_map {
                if let Value::String(val) = value {
                    let tag = aws_sdk_iam::types::Tag::builder()
                        .key(key)
                        .value(val)
                        .build()
                        .map_err(|e| {
                            ProviderError::new(format!("Failed to build tag: {}", e))
                                .for_resource(resource.id.clone())
                        })?;
                    req = req.tags(tag);
                }
            }
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to create IAM role")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        self.read_iam_role(&resource.id, Some(&role_name)).await
    }

    /// Update an IAM Role
    pub(crate) async fn update_iam_role(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        // Update assume role policy document
        if let Some(Value::String(policy_doc)) = to.attributes.get("assume_role_policy_document") {
            self.iam_client
                .update_assume_role_policy()
                .role_name(identifier)
                .policy_document(policy_doc)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to update assume role policy")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        }

        // Update description and max_session_duration via update_role
        let mut needs_update = false;
        let mut req = self.iam_client.update_role().role_name(identifier);

        if let Some(Value::String(desc)) = to.attributes.get("description") {
            req = req.description(desc);
            needs_update = true;
        }

        if let Some(Value::Int(duration)) = to.attributes.get("max_session_duration") {
            req = req.max_session_duration(*duration as i32);
            needs_update = true;
        }

        if needs_update {
            req.send().await.map_err(|e| {
                ProviderError::new("Failed to update IAM role")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        // Update tags
        self.apply_iam_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;

        self.read_iam_role(&id, Some(identifier)).await
    }

    /// Delete an IAM Role
    pub(crate) async fn delete_iam_role(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.iam_client
            .delete_role()
            .role_name(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete IAM role")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }

    /// Apply IAM tags (create/delete tag differences)
    async fn apply_iam_tags(
        &self,
        id: &ResourceId,
        role_name: &str,
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
            let mut req = self.iam_client.untag_role().role_name(role_name);
            for key in &keys_to_remove {
                req = req.tag_keys(key);
            }
            req.send().await.map_err(|e| {
                ProviderError::new("Failed to untag IAM role")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        // Tags to add/update
        let mut tags_to_add = Vec::new();
        for (key, value) in &desired_tags {
            if let Value::String(val) = value {
                let should_add = match current_tags.get(key) {
                    Some(Value::String(current_val)) => current_val != val,
                    _ => true,
                };
                if should_add {
                    let tag = aws_sdk_iam::types::Tag::builder()
                        .key(key)
                        .value(val)
                        .build()
                        .map_err(|e| {
                            ProviderError::new(format!("Failed to build tag: {}", e))
                                .for_resource(id.clone())
                        })?;
                    tags_to_add.push(tag);
                }
            }
        }

        if !tags_to_add.is_empty() {
            let mut req = self.iam_client.tag_role().role_name(role_name);
            for tag in tags_to_add {
                req = req.tags(tag);
            }
            req.send().await.map_err(|e| {
                ProviderError::new("Failed to tag IAM role")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        Ok(())
    }
}
