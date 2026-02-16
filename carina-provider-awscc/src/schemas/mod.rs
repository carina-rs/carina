//! AWS Cloud Control resource schema definitions

pub mod awscc_types;
pub mod generated;

use carina_core::schema::ResourceSchema;

/// Returns all AWS Cloud Control schemas
/// Auto-generated from CloudFormation schemas
pub fn all_schemas() -> Vec<ResourceSchema> {
    generated::schemas()
}
