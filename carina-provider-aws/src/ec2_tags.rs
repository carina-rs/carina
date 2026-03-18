//! EC2 tag helper functions

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{ResourceId, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Extract tags from EC2 tag list into a Value::Map
    pub(crate) fn ec2_tags_to_value(tags: &[aws_sdk_ec2::types::Tag]) -> Option<Value> {
        let mut tag_map = HashMap::new();
        for tag in tags {
            if let (Some(key), Some(value)) = (tag.key(), tag.value()) {
                tag_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if tag_map.is_empty() {
            None
        } else {
            Some(Value::Map(tag_map))
        }
    }

    /// Build EC2 Tag list from Value::Map
    pub(crate) fn value_to_ec2_tags(value: &Value) -> Vec<aws_sdk_ec2::types::Tag> {
        let mut tags = Vec::new();
        if let Value::Map(map) = value {
            for (key, val) in map {
                if let Value::String(v) = val {
                    tags.push(aws_sdk_ec2::types::Tag::builder().key(key).value(v).build());
                }
            }
        }
        tags
    }

    /// Apply tags to an EC2 resource
    ///
    /// When `from_attributes` is provided, tags that exist in `from` but not in `to`
    /// will be deleted from the resource.
    pub(crate) async fn apply_ec2_tags(
        &self,
        resource_id: &ResourceId,
        ec2_resource_id: &str,
        attributes: &HashMap<String, Value>,
        from_attributes: Option<&HashMap<String, Value>>,
    ) -> ProviderResult<()> {
        // Delete tags that were removed (present in from but not in to)
        if let Some(from_attrs) = from_attributes {
            let old_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(old_map)) = from_attrs.get("tags") {
                    old_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let new_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(new_map)) = attributes.get("tags") {
                    new_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let removed_keys: Vec<&String> = old_keys.difference(&new_keys).copied().collect();
            if !removed_keys.is_empty() {
                let mut req = self.ec2_client.delete_tags().resources(ec2_resource_id);
                for key in removed_keys {
                    req = req.tags(aws_sdk_ec2::types::Tag::builder().key(key.as_str()).build());
                }
                req.send().await.map_err(|e| {
                    ProviderError::new("Failed to delete tags")
                        .with_cause(e)
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        // Add/update tags
        if let Some(tag_value) = attributes.get("tags") {
            let tags = Self::value_to_ec2_tags(tag_value);
            if !tags.is_empty() {
                let mut req = self.ec2_client.create_tags().resources(ec2_resource_id);
                for tag in tags {
                    req = req.tags(tag);
                }
                req.send().await.map_err(|e| {
                    ProviderError::new("Failed to tag resource")
                        .with_cause(e)
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ec2_tags_to_value tests ---

    #[test]
    fn test_ec2_tags_to_value_empty() {
        let tags: Vec<aws_sdk_ec2::types::Tag> = vec![];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    #[test]
    fn test_ec2_tags_to_value_single_tag() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("my-resource")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        assert!(result.is_some());
        if let Some(Value::Map(map)) = result {
            assert_eq!(
                map.get("Name"),
                Some(&Value::String("my-resource".to_string()))
            );
        } else {
            panic!("Expected Value::Map");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_multiple_tags() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("test")
                .build(),
            aws_sdk_ec2::types::Tag::builder()
                .key("Environment")
                .value("production")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        if let Some(Value::Map(map)) = result {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("Name"), Some(&Value::String("test".to_string())));
            assert_eq!(
                map.get("Environment"),
                Some(&Value::String("production".to_string()))
            );
        } else {
            panic!("Expected Value::Map with 2 entries");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_missing_key_or_value() {
        // Tag with no key set
        let tags = vec![aws_sdk_ec2::types::Tag::builder().build()];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    // --- value_to_ec2_tags tests ---

    #[test]
    fn test_value_to_ec2_tags_from_map() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Env".to_string(), Value::String("prod".to_string())),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 2);
        // Check both tags exist (order not guaranteed from HashMap)
        let tag_map: HashMap<String, String> = tags
            .iter()
            .map(|t| {
                (
                    t.key().unwrap_or("").to_string(),
                    t.value().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(tag_map.get("Name"), Some(&"test".to_string()));
        assert_eq!(tag_map.get("Env"), Some(&"prod".to_string()));
    }

    #[test]
    fn test_value_to_ec2_tags_non_map_value() {
        let value = Value::String("not a map".to_string());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
    }

    #[test]
    fn test_value_to_ec2_tags_non_string_values_skipped() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Count".to_string(), Value::Int(42)),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].key(), Some("Name"));
        assert_eq!(tags[0].value(), Some("test"));
    }

    #[test]
    fn test_value_to_ec2_tags_empty_map() {
        let value = Value::Map(HashMap::new());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
    }
}
