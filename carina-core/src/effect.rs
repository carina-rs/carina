//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when the Interpreter executes the Effect.

use serde::{Deserialize, Serialize};

use crate::resource::{LifecycleConfig, Resource, ResourceId, State};

/// A dependent resource that must be updated during a create_before_destroy replacement.
///
/// When a resource is replaced with create_before_destroy, dependent resources that
/// reference the replaced resource's computed attributes need to be updated between
/// the create (new) and delete (old) steps. The `to` field retains unresolved
/// `ResourceRef` values so that the apply phase can re-resolve them using the
/// newly created resource's state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CascadingUpdate {
    pub id: ResourceId,
    pub from: Box<State>,
    pub to: Resource,
}

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

    /// Replace a resource (delete then create) due to create-only property changes
    Replace {
        id: ResourceId,
        from: Box<State>,
        to: Resource,
        #[serde(default)]
        lifecycle: LifecycleConfig,
        /// Which create-only attributes forced the replacement
        changed_create_only: Vec<String>,
        /// Dependent resources to update between create and delete (create_before_destroy only)
        #[serde(default)]
        cascading_updates: Vec<CascadingUpdate>,
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
            Effect::Replace { .. } => "replace",
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
            Effect::Replace { id, .. } => id,
            Effect::Delete { id, .. } => id,
        }
    }

    /// Returns a reference to the resource for this effect, if it has one.
    /// Delete effects have no resource.
    pub fn resource(&self) -> Option<&Resource> {
        match self {
            Effect::Create(resource) => Some(resource),
            Effect::Update { to, .. } => Some(to),
            Effect::Replace { to, .. } => Some(to),
            Effect::Read { resource } => Some(resource),
            Effect::Delete { .. } => None,
        }
    }

    /// Returns the binding name for this effect's resource, if it has one.
    pub fn binding_name(&self) -> Option<String> {
        use crate::resource::Value;
        self.resource().and_then(|r| {
            r.attributes.get("_binding").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
        })
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
        let resource = Resource::new("s3.bucket", "my-bucket");
        let effect = Effect::Create(resource);
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource = Resource::new("s3.bucket", "my-bucket").with_read_only(true);
        let effect = Effect::Read {
            resource: resource.clone(),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn resource_returns_some_for_create() {
        let resource = Resource::new("s3.bucket", "my-bucket");
        let effect = Effect::Create(resource.clone());
        assert_eq!(effect.resource().unwrap().id, resource.id);
    }

    #[test]
    fn resource_returns_none_for_delete() {
        let effect = Effect::Delete {
            id: ResourceId::new("test", "a"),
            identifier: "id-123".to_string(),
            lifecycle: LifecycleConfig::default(),
        };
        assert!(effect.resource().is_none());
    }

    #[test]
    fn binding_name_returns_binding() {
        use crate::resource::Value;
        let resource = Resource::new("test", "my_binding")
            .with_attribute("_binding", Value::String("my_binding".to_string()));
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), Some("my_binding".to_string()));
    }

    #[test]
    fn binding_name_returns_none_without_binding() {
        use crate::resource::Value;
        let resource = Resource::new("test", "no_binding")
            .with_attribute("name", Value::String("test".to_string()));
        let effect = Effect::Create(resource);
        assert_eq!(effect.binding_name(), None);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let effects = vec![
            Effect::Create(Resource::new("s3.bucket", "my-bucket")),
            Effect::Read {
                resource: Resource::new("s3.bucket", "existing").with_read_only(true),
            },
            Effect::Update {
                id: ResourceId::new("s3.bucket", "my-bucket"),
                from: Box::new(State::existing(
                    ResourceId::new("s3.bucket", "my-bucket"),
                    HashMap::from([(
                        "versioning".to_string(),
                        Value::String("Disabled".to_string()),
                    )]),
                )),
                to: Resource::new("s3.bucket", "my-bucket")
                    .with_attribute("versioning", Value::String("Enabled".to_string())),
            },
            Effect::Replace {
                id: ResourceId::new("ec2.vpc", "my-vpc"),
                from: Box::new(State::existing(
                    ResourceId::new("ec2.vpc", "my-vpc"),
                    HashMap::from([(
                        "cidr_block".to_string(),
                        Value::String("10.0.0.0/16".to_string()),
                    )]),
                )),
                to: Resource::new("ec2.vpc", "my-vpc")
                    .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
                lifecycle: LifecycleConfig::default(),
                changed_create_only: vec!["cidr_block".to_string()],
                cascading_updates: vec![],
            },
            Effect::Delete {
                id: ResourceId::new("s3.bucket", "old-bucket"),
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
