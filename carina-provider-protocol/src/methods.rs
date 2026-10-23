//! Per-method request params and response result types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::*;

// -- provider_info --

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderInfoResult {
    pub info: ProviderInfo,
}

// -- validate_config --

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateConfigParams {
    pub attributes: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateConfigResult {
    pub error: Option<String>,
}

// -- schemas --

#[derive(Debug, Serialize, Deserialize)]
pub struct SchemasResult {
    pub schemas: Vec<ResourceSchema>,
}

// -- initialize --

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeParams {
    pub attributes: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    pub ok: bool,
}

// -- read --

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadParams {
    pub id: ResourceId,
    /// Validated, non-empty provider identifier of the managed resource.
    pub identifier: AppliedIdentifier,
    /// Per-operation request record. Present so future fields (e.g.
    /// freshness hint) can be added without breaking existing
    /// providers. Defaults to an empty record on the wire.
    #[serde(default)]
    pub request: ReadRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadResult {
    pub outcome: ReadOutcome,
}

// -- create --

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateParams {
    pub id: ResourceId,
    pub request: CreateRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateResult {
    pub outcome: CreateOutcome,
}

// -- update --

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateParams {
    pub id: ResourceId,
    pub identifier: AppliedIdentifier,
    pub request: UpdateRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateResult {
    pub outcome: UpdateOutcome,
}

// -- delete --

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteParams {
    pub id: ResourceId,
    pub identifier: AppliedIdentifier,
    pub request: DeleteRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteResult {
    pub ok: bool,
}

// -- normalize_desired --

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeDesiredParams {
    pub resources: Vec<Resource>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeDesiredResult {
    pub resources: Vec<Resource>,
}

// -- normalize_state --

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeStateParams {
    pub states: HashMap<String, PersistedState>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeStateResult {
    pub states: HashMap<String, PersistedState>,
}

// -- hydrate_read_state --

#[derive(Debug, Serialize, Deserialize)]
pub struct HydrateReadStateParams {
    pub states: HashMap<String, PersistedState>,
    pub saved_attrs: HashMap<String, HashMap<String, PersistedValue>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HydrateReadStateResult {
    pub states: HashMap<String, PersistedState>,
}
