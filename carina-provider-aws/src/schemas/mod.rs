//! AWS resource schema definitions

pub mod generated;
pub mod types;

use carina_core::schema::ResourceSchema;

/// Returns all AWS schemas
pub fn all_schemas() -> Vec<ResourceSchema> {
    generated::configs().into_iter().map(|c| c.schema).collect()
}
