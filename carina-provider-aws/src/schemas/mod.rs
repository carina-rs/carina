//! AWS resource schema definitions

pub mod s3;
pub mod vpc;

use carina_core::schema::ResourceSchema;

/// Returns all AWS schemas
pub fn all_schemas() -> Vec<ResourceSchema> {
    let mut schemas = Vec::new();
    schemas.extend(s3::schemas());
    schemas.extend(vpc::schemas());
    schemas
}
