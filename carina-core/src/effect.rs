//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when the Interpreter executes the Effect.

use serde::{Deserialize, Serialize};

use crate::resource::{LifecycleConfig, Resource, ResourceId, State};

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// Read the current state of a resource (data source)
    Read { resource: Resource },

    /// Create a new resource
    Create(Resource),

    /// Update an existing resource
    Update {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
    },

    /// Delete a resource
    Delete {
        id: ResourceId,
        identifier: String,
        #[serde(default)]
        lifecycle: LifecycleConfig,
    },
}

impl Effect {
    /// Returns the kind of Effect as a string (for display)
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Read { .. } => "read",
            Effect::Create(_) => "create",
            Effect::Update { .. } => "update",
            Effect::Delete { .. } => "delete",
        }
    }

    /// Returns whether this Effect causes a mutation
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Effect::Read { .. })
    }

    /// Returns the resource ID for this effect
    pub fn resource_id(&self) -> &ResourceId {
        match self {
            Effect::Read { resource } => &resource.id,
            Effect::Create(r) => &r.id,
            Effect::Update { id, .. } => id,
            Effect::Delete { id, .. } => id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_is_not_mutating() {
        let resource = Resource::new("test", "example").with_read_only(true);
        let effect = Effect::Read { resource };
        assert!(!effect.is_mutating());
    }

    #[test]
    fn create_is_mutating() {
        let resource = Resource::new("s3_bucket", "my-bucket");
        let effect = Effect::Create(resource);
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource = Resource::new("s3_bucket", "my-bucket").with_read_only(true);
        let effect = Effect::Read {
            resource: resource.clone(),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let effects = vec![
            Effect::Create(Resource::new("s3_bucket", "my-bucket")),
            Effect::Read {
                resource: Resource::new("s3_bucket", "existing").with_read_only(true),
            },
            Effect::Update {
                id: ResourceId::new("s3_bucket", "my-bucket"),
                from: Box::new(State::existing(
                    ResourceId::new("s3_bucket", "my-bucket"),
                    HashMap::from([(
                        "versioning".to_string(),
                        Value::String("Disabled".to_string()),
                    )]),
                )),
                to: Resource::new("s3_bucket", "my-bucket")
                    .with_attribute("versioning", Value::String("Enabled".to_string())),
            },
            Effect::Delete {
                id: ResourceId::new("s3_bucket", "old-bucket"),
                identifier: "old-bucket".to_string(),
                lifecycle: LifecycleConfig::default(),
            },
        ];

        for effect in effects {
            let json = serde_json::to_string(&effect).unwrap();
            let deserialized: Effect = serde_json::from_str(&json).unwrap();
            assert_eq!(effect, deserialized, "Round-trip failed for {:?}", effect);
        }
    }
}
