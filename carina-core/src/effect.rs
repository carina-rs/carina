//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when the Interpreter executes the Effect.

use crate::resource::{Resource, ResourceId, State};

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Read the current state of a resource
    Read(ResourceId),

    /// Create a new resource
    Create(Resource),

    /// Update an existing resource
    Update {
        id: ResourceId,
        from: State,
        to: Resource,
    },

    /// Delete a resource
    Delete(ResourceId),
}

impl Effect {
    /// Returns the kind of Effect as a string (for display)
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Read(_) => "read",
            Effect::Create(_) => "create",
            Effect::Update { .. } => "update",
            Effect::Delete(_) => "delete",
        }
    }

    /// Returns whether this Effect causes a mutation
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Effect::Read(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_is_not_mutating() {
        let effect = Effect::Read(ResourceId::new("test", "example"));
        assert!(!effect.is_mutating());
    }

    #[test]
    fn create_is_mutating() {
        let resource = Resource::new("s3_bucket", "my-bucket");
        let effect = Effect::Create(resource);
        assert!(effect.is_mutating());
    }
}
