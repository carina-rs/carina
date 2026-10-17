use std::collections::HashMap;

use carina_core::parser::{BackendConfig, SavedProviderConfigs};
use carina_core::plan::Plan;
use carina_core::provider::{PersistedAttributes, Provider, ReadRequest};
use carina_core::resource::{Composition, DataSource, Resource, ResourceId, State, Value};

use super::{ApplyTarget, saved_plan_secret_rehydration_error};
use crate::commands::plan::{
    CurrentStateEntry, PlanFile, PlanWaitBinding, ResolvedProviderInputs, UpstreamSource,
};
use crate::error::AppError;

pub(super) type PlannedSecretHashes = HashMap<ResourceId, PersistedAttributes>;

fn planned_secret_hashes(resources: &[Resource]) -> Result<PlannedSecretHashes, AppError> {
    resources
        .iter()
        .filter(|resource| {
            resource
                .attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
        })
        .map(|resource| {
            PersistedAttributes::from_saved_plan_attributes(&resource.attributes)
                .map(|attributes| (resource.id.clone(), attributes))
                .map_err(AppError::from)
        })
        .collect()
}

/// Historical provider/hash address for each saved-plan resource address.
///
/// A `moved` block rewrites the runtime state key to its destination, while
/// the persisted secret hash and remote object still belong to the source
/// address until execution completes. This typed map keeps both drift reads
/// and hash verification on that source address.
#[derive(Default)]
pub(crate) struct HistoricalResourceAddresses {
    moved_origins: HashMap<ResourceId, ResourceId>,
}

impl HistoricalResourceAddresses {
    fn from_plan(plan: &Plan) -> Self {
        let moved_origins = plan
            .effects()
            .iter()
            .filter_map(|effect| match effect {
                carina_core::effect::Effect::Move { from, to } => {
                    Some((to.as_inner().clone(), from.as_inner().clone()))
                }
                _ => None,
            })
            .collect();
        Self { moved_origins }
    }

    pub(super) fn origin_for(&self, id: &ResourceId) -> ResourceId {
        self.moved_origins
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.clone())
    }
}

/// Result of the one typed placeholder inventory performed after decoding.
///
/// Both payload fields are private. The parent apply module can inspect only
/// the metadata needed to select a backend; it cannot recover a raw
/// deserialized `PlanFile` and route it to execution.
pub(super) enum InspectedPlanFile {
    Verified(RehydratedPlanFile),
    NeedsSecretVerification(PendingPlanFile),
}

impl InspectedPlanFile {
    pub(super) fn inspect(plan_file: PlanFile) -> Self {
        if plan_file.contains_redacted_secret_placeholders() {
            Self::NeedsSecretVerification(PendingPlanFile { plan_file })
        } else {
            Self::Verified(RehydratedPlanFile {
                plan_file,
                planned_secret_hashes: HashMap::new(),
            })
        }
    }

    pub(super) fn backend_config(&self) -> Option<&BackendConfig> {
        match self {
            Self::Verified(plan) => plan.backend_config(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.backend_config.as_ref(),
        }
    }

    pub(super) fn needs_secret_verification(&self) -> bool {
        matches!(self, Self::NeedsSecretVerification(_))
    }

    pub(super) fn state_lineage(&self) -> Option<&str> {
        match self {
            Self::Verified(plan) => plan.state_lineage(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.state_lineage.as_deref(),
        }
    }

    pub(super) fn state_serial(&self) -> Option<u64> {
        match self {
            Self::Verified(plan) => plan.state_serial(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.state_serial,
        }
    }
}

/// Deserialized saved plan whose typed placeholders still need fresh desired
/// values. There is intentionally no accessor for the inner plan.
pub(super) struct PendingPlanFile {
    plan_file: PlanFile,
}

impl PendingPlanFile {
    pub(super) fn verify_desired(
        self,
        parsed: &carina_core::parser::InferredFile,
        provider_inputs: &ResolvedProviderInputs,
        apply_target: ApplyTarget<'_>,
    ) -> Result<DesiredPlanFile, AppError> {
        let planned_secret_hashes = planned_secret_hashes(&self.plan_file.sorted_resources)?;
        let plan_file = super::rehydrate_desired_plan_file(
            self.plan_file,
            parsed,
            provider_inputs,
            apply_target,
        )?;
        Ok(DesiredPlanFile::NeedsHistoricalVerification(
            DesiredVerifiedPlanFile {
                plan_file,
                planned_secret_hashes,
            },
        ))
    }
}

/// Common post-desired-verification state. Placeholder-free plans are already
/// final; secret-bearing plans retain a sealed historical-verification step.
pub(super) enum DesiredPlanFile {
    Verified(RehydratedPlanFile),
    NeedsHistoricalVerification(DesiredVerifiedPlanFile),
}

impl DesiredPlanFile {
    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        match self {
            Self::Verified(plan) => plan.provider_configs(),
            Self::NeedsHistoricalVerification(plan) => plan.provider_configs(),
        }
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        match self {
            Self::Verified(plan) => plan.sorted_resources(),
            Self::NeedsHistoricalVerification(plan) => plan.sorted_resources(),
        }
    }

    pub(super) fn current_states(&self) -> &[CurrentStateEntry] {
        match self {
            Self::Verified(plan) => plan.current_states(),
            Self::NeedsHistoricalVerification(plan) => plan.current_states(),
        }
    }

    pub(super) fn historical_resource_addresses(&self) -> HistoricalResourceAddresses {
        let plan = match self {
            Self::Verified(plan) => plan.plan(),
            Self::NeedsHistoricalVerification(plan) => &plan.plan_file.plan,
        };
        HistoricalResourceAddresses::from_plan(plan)
    }

    pub(super) async fn verify_historical(
        self,
        provider: &dyn Provider,
        actual_states: &mut HashMap<ResourceId, State>,
        verified_planned_states: &mut HashMap<ResourceId, State>,
        historical_addresses: &HistoricalResourceAddresses,
        apply_target: ApplyTarget<'_>,
    ) -> Result<RehydratedPlanFile, AppError> {
        match self {
            Self::Verified(plan) => Ok(plan),
            Self::NeedsHistoricalVerification(plan) => {
                plan.verify_historical(
                    provider,
                    actual_states,
                    verified_planned_states,
                    historical_addresses,
                    apply_target,
                )
                .await
            }
        }
    }
}

/// A saved plan whose source-derived desired values and provider
/// configuration have been verified. Historical placeholders may still need
/// fresh provider reads, so this type deliberately cannot reach the executor.
pub(super) struct DesiredVerifiedPlanFile {
    plan_file: PlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl DesiredVerifiedPlanFile {
    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        &self.plan_file.provider_configs
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        &self.plan_file.sorted_resources
    }

    pub(super) fn current_states(&self) -> &[CurrentStateEntry] {
        &self.plan_file.current_states
    }

    /// Verify every historical placeholder against a fresh provider read and
    /// mint the sole saved-plan executor gate.
    pub(super) async fn verify_historical(
        mut self,
        provider: &dyn Provider,
        actual_states: &mut HashMap<ResourceId, State>,
        verified_planned_states: &mut HashMap<ResourceId, State>,
        historical_addresses: &HistoricalResourceAddresses,
        apply_target: ApplyTarget<'_>,
    ) -> Result<RehydratedPlanFile, AppError> {
        let redacted_states: HashMap<_, _> = self
            .plan_file
            .current_states
            .iter()
            .map(|entry| (entry.id.clone(), entry.state.clone()))
            .collect();
        for entry in &mut self.plan_file.current_states {
            if !entry
                .state
                .attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
            {
                continue;
            }

            if let Some(verified) = verified_planned_states.get(&entry.id) {
                entry.state = verified.clone();
                continue;
            }

            // Orphans do not participate in ordinary drift reads. Read them
            // here because their historical secret still needs verification.
            if !actual_states.contains_key(&entry.id) {
                let historical_id = historical_addresses.origin_for(&entry.id);
                let mut actual = provider
                    .read(
                        &historical_id,
                        entry.state.identifier.as_deref(),
                        ReadRequest,
                    )
                    .await
                    .map_err(AppError::Provider)?;
                if !actual.exists {
                    return Err(super::saved_plan_secret_orphan_vanished_error(
                        entry.id.to_string(),
                        apply_target,
                    ));
                }
                actual.id = entry.id.clone();
                actual_states.insert(entry.id.clone(), actual);
            }
            let actual = actual_states.get(&entry.id).ok_or_else(|| {
                super::saved_plan_secret_shape_error(entry.id.to_string(), apply_target)
            })?;
            entry.state =
                carina_core::value::rehydrate_state_secrets_from_plaintext(&entry.state, actual)
                    .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
            verified_planned_states.insert(entry.id.clone(), entry.state.clone());
        }
        self.plan_file.plan =
            carina_core::value::rehydrate_plan_historical_secrets_from_verified_states(
                &self.plan_file.plan,
                &redacted_states,
                verified_planned_states,
            )
            .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;

        Ok(RehydratedPlanFile {
            plan_file: self.plan_file,
            planned_secret_hashes: self.planned_secret_hashes,
        })
    }
}

/// A saved plan that has crossed both typed secret-verification boundaries.
///
/// The payload is deliberately private to this child module. This is the only
/// type accepted by the saved-plan executor gateway.
pub(super) struct RehydratedPlanFile {
    plan_file: PlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl RehydratedPlanFile {
    pub(super) fn plan(&self) -> &Plan {
        &self.plan_file.plan
    }

    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        &self.plan_file.provider_configs
    }

    pub(super) fn backend_config(&self) -> Option<&BackendConfig> {
        self.plan_file.backend_config.as_ref()
    }

    pub(super) fn state_lineage(&self) -> Option<&str> {
        self.plan_file.state_lineage.as_deref()
    }

    pub(super) fn state_serial(&self) -> Option<u64> {
        self.plan_file.state_serial
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        &self.plan_file.sorted_resources
    }

    pub(super) fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        &self.planned_secret_hashes
    }

    pub(super) fn unresolved_resources(&self) -> &[Resource] {
        &self.plan_file.unresolved_resources
    }

    pub(super) fn compositions(&self) -> &[Composition] {
        &self.plan_file.compositions
    }

    pub(super) fn data_sources(&self) -> &[DataSource] {
        &self.plan_file.data_sources
    }

    pub(super) fn data_source_origins(&self) -> &[DataSource] {
        &self.plan_file.data_source_origins
    }

    pub(super) fn current_states(&self) -> &[CurrentStateEntry] {
        &self.plan_file.current_states
    }

    pub(super) fn upstream_snapshot(&self) -> &HashMap<String, HashMap<String, Value>> {
        &self.plan_file.upstream_snapshot
    }

    pub(super) fn upstream_sources(&self) -> &[UpstreamSource] {
        &self.plan_file.upstream_sources
    }

    pub(super) fn wait_bindings(&self) -> &[PlanWaitBinding] {
        &self.plan_file.wait_bindings
    }
}
