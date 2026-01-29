//! AWS Cloud Control resource schema definitions

pub mod vpc;

use carina_core::schema::ResourceSchema;

/// Returns all AWS Cloud Control schemas
pub fn all_schemas() -> Vec<ResourceSchema> {
    vec![vpc::vpc_schema()]
}
