//! Permanent name override decision primitives.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::resource::ResourceId;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NameOverride {
    pub temp_value: String,
    /// The DSL value at the time the override was recorded. `None`
    /// indicates a state file migrated from v7 or earlier where the
    /// original was not captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_value: Option<String>,
}

/// Minimal state-like surface for reading permanent name overrides.
///
/// The trait remains in core so state backends can implement it without
/// depending on CLI validation wiring.
pub trait NameOverrideSource {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>>;
}

impl NameOverrideSource for HashMap<ResourceId, HashMap<String, NameOverride>> {
    fn name_overrides_for(
        &self,
        resource_id: &ResourceId,
    ) -> Option<&HashMap<String, NameOverride>> {
        self.get(resource_id)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NameOverrideDeserialize {
    Legacy(String),
    Current {
        temp_value: String,
        #[serde(default)]
        original_value: Option<String>,
    },
}

impl<'de> Deserialize<'de> for NameOverride {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match NameOverrideDeserialize::deserialize(deserializer)? {
            NameOverrideDeserialize::Legacy(temp_value) => Ok(Self {
                temp_value,
                original_value: None,
            }),
            NameOverrideDeserialize::Current {
                temp_value,
                original_value,
            } => Ok(Self {
                temp_value,
                original_value,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyDecision {
    Apply,
    Skip,
    ApplyWithUnknownDsl,
    ApplyLegacy,
}

pub fn should_apply_override(
    current_dsl_value: Option<&str>,
    override_: &NameOverride,
) -> ApplyDecision {
    match (&override_.original_value, current_dsl_value) {
        (Some(orig), Some(cur)) if orig == cur => ApplyDecision::Apply,
        (Some(_), Some(_)) => ApplyDecision::Skip,
        (Some(_), None) => ApplyDecision::ApplyWithUnknownDsl,
        (None, _) => ApplyDecision::ApplyLegacy,
    }
}
