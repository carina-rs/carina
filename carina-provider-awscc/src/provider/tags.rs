//! Tag conversion helpers for CloudFormation format.
//!
//! CloudFormation uses `[{"Key": "k", "Value": "v"}, ...]` format for tags,
//! while the DSL uses `{ k = "v", ... }` map format. This module handles
//! bidirectional conversion between these formats.

use std::collections::HashMap;

use carina_core::resource::Value;
use serde_json::json;

use super::AwsccProvider;

impl AwsccProvider {
    /// Build tags array for CloudFormation format
    pub(crate) fn build_tags(&self, user_tags: Option<&Value>) -> Vec<serde_json::Value> {
        let mut tags = Vec::new();
        if let Some(Value::Map(user_tags)) = user_tags {
            for (key, value) in user_tags {
                if let Value::String(v) = value {
                    tags.push(json!({"Key": key, "Value": v}));
                }
            }
        }
        tags
    }

    /// Parse tags from CloudFormation format to map
    pub(crate) fn parse_tags(&self, tags_array: &[serde_json::Value]) -> HashMap<String, Value> {
        let mut tags_map = HashMap::new();
        for tag in tags_array {
            if let (Some(key), Some(value)) = (
                tag.get("Key").and_then(|v| v.as_str()),
                tag.get("Value").and_then(|v| v.as_str()),
            ) {
                tags_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        tags_map
    }
}
