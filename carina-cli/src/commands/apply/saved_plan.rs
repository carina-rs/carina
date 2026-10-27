use std::collections::{HashMap, HashSet};

use carina_core::parser::{BackendConfig, InferredExportParam, SavedProviderConfigs};
use carina_core::plan::Plan;
use carina_core::provider::{AppliedIdentifier, PersistedAttributes, Provider, ReadRequest};
use carina_core::resource::{Composition, Current, DataSource, Resource, ResourceId, State, Value};

use super::{ApplyTarget, saved_plan_secret_rehydration_error};
use crate::commands::plan::{
    MovedOrigins, PlanFile, PlanWaitBinding, ReloadedCurrentStateEntry, ReloadedPlanFile,
    ResolvedProviderInputs, UpstreamSource,
};
use crate::error::AppError;
use crate::wiring::ExpandedCanonicalResources;

pub(super) type PlannedSecretHashes = HashMap<ResourceId, PersistedAttributes>;

fn planned_secret_hashes(resources: &[Resource]) -> PlannedSecretHashes {
    resources
        .iter()
        .filter(|resource| {
            resource
                .attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
        })
        .map(|resource| {
            (
                resource.id.clone(),
                PersistedAttributes::from_saved_plan_attributes(&resource.attributes),
            )
        })
        .collect()
}

pub(crate) type HistoricalResourceAddresses = MovedOrigins;

struct HistoricalStateVerifier<'a> {
    provider: &'a dyn Provider,
    actual_states: &'a mut HashMap<ResourceId, State>,
    verified_planned_states: &'a mut HashMap<ResourceId, State>,
    historical_addresses: &'a HistoricalResourceAddresses,
    apply_target: ApplyTarget<'a>,
}

impl HistoricalStateVerifier<'_> {
    async fn verify(
        &mut self,
        id: &ResourceId,
        state: &State,
        provider_identifier: Option<&AppliedIdentifier>,
    ) -> Result<Option<State>, AppError> {
        if !state
            .attributes
            .values()
            .any(carina_core::value::contains_redacted_secret)
        {
            return Ok(None);
        }

        if let Some(verified) = self.verified_planned_states.get(id) {
            return Ok(Some(verified.clone()));
        }

        // Orphans do not participate in ordinary drift reads. Read them here
        // because their historical secret still needs verification.
        if !self.actual_states.contains_key(id) {
            let historical_id = self.historical_addresses.origin_for(id);
            let identifier = provider_identifier.ok_or_else(|| {
                AppError::Validation(format!(
                    "saved plan resource {id} has no valid provider identifier; create a new plan before applying"
                ))
            })?;
            let mut actual = self
                .provider
                .read(historical_id, identifier, ReadRequest)
                .await
                .map_err(AppError::Provider)?
                .into_state();
            if !actual.exists {
                return Err(super::saved_plan_secret_orphan_vanished_error(
                    id.to_string(),
                    self.apply_target,
                ));
            }
            actual.id = id.clone();
            self.actual_states.insert(id.clone(), actual);
        }
        let actual = self.actual_states.get(id).ok_or_else(|| {
            super::saved_plan_secret_shape_error(id.to_string(), self.apply_target)
        })?;
        let verified = carina_core::value::rehydrate_state_secrets_from_plaintext(state, actual)
            .map_err(|error| saved_plan_secret_rehydration_error(error, self.apply_target))?;
        self.verified_planned_states
            .insert(id.clone(), verified.clone());
        Ok(Some(verified))
    }
}

/// Result of the one typed placeholder inventory performed after decoding.
///
/// Both payload fields are private. The parent apply module can inspect only
/// the metadata needed to select a backend; it cannot recover a raw
/// deserialized `PlanFile` and route it to execution.
pub(super) enum InspectedPlanFile {
    Verified(VerifiedPlanFile),
    NeedsSecretVerification(PendingPlanFile),
}

/// Decoded placeholder-free plan. It still cannot expose managed current
/// state until its in-flight markers are reconciled under the state lock.
pub(super) struct VerifiedPlanFile {
    plan_file: PlanFile,
}

impl InspectedPlanFile {
    pub(super) fn inspect(plan_file: PlanFile) -> Self {
        if plan_file.contains_redacted_secret_placeholders() {
            Self::NeedsSecretVerification(PendingPlanFile { plan_file })
        } else {
            Self::Verified(VerifiedPlanFile { plan_file })
        }
    }

    pub(super) fn backend_config(&self) -> Option<&BackendConfig> {
        match self {
            Self::Verified(plan) => plan.plan_file.backend_config.as_ref(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.backend_config.as_ref(),
        }
    }

    pub(super) fn needs_secret_verification(&self) -> bool {
        matches!(self, Self::NeedsSecretVerification(_))
    }

    pub(super) fn state_lineage(&self) -> Option<&str> {
        match self {
            Self::Verified(plan) => plan.plan_file.state_lineage.as_deref(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.state_lineage.as_deref(),
        }
    }

    pub(super) fn state_serial(&self) -> Option<u64> {
        match self {
            Self::Verified(plan) => plan.plan_file.state_serial,
            Self::NeedsSecretVerification(plan) => plan.plan_file.state_serial,
        }
    }

    pub(super) fn data_source_origins(&self) -> &[DataSource] {
        match self {
            Self::Verified(plan) => &plan.plan_file.data_source_origins,
            Self::NeedsSecretVerification(plan) => &plan.plan_file.data_source_origins,
        }
    }

    pub(super) fn reload_in_flight_from_locked_state(
        self,
        state_file: Option<&carina_state::StateFile>,
    ) -> Result<ReloadedInspectedPlanFile, AppError> {
        match self {
            Self::Verified(plan) => plan
                .plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(|plan_file| {
                    ReloadedInspectedPlanFile::Verified(RehydratedPlanFile {
                        plan_file,
                        planned_secret_hashes: HashMap::new(),
                    })
                })
                .map_err(AppError::Config),
            Self::NeedsSecretVerification(plan) => plan
                .plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(|plan_file| {
                    ReloadedInspectedPlanFile::NeedsSecretVerification(ReloadedPendingPlanFile {
                        plan_file,
                    })
                })
                .map_err(AppError::Config),
        }
    }
}

/// Saved-plan state after the only locked-state reload transition.
pub(super) enum ReloadedInspectedPlanFile {
    Verified(RehydratedPlanFile),
    NeedsSecretVerification(ReloadedPendingPlanFile),
}

/// Deserialized saved plan whose typed placeholders still need fresh desired
/// values. There is intentionally no accessor for the inner plan.
pub(super) struct PendingPlanFile {
    plan_file: PlanFile,
}

/// Reloaded saved plan whose desired secret placeholders still need fresh
/// source values.
pub(super) struct ReloadedPendingPlanFile {
    plan_file: ReloadedPlanFile,
}

pub(super) struct ExpansionPriorStates {
    pub managed: HashMap<ResourceId, Current>,
    pub data_sources: HashMap<ResourceId, State>,
    pub already_refreshed: HashSet<ResourceId>,
}

/// Fresh source values whose managed-resource slice is proven to have crossed
/// the same deferred-`for` expansion seam as plan recording.
///
/// The fields are private and the production constructor consumes the opaque
/// [`ExpandedCanonicalResources`] witness, so a pending saved plan cannot be
/// verified accidentally against the parser's pre-expansion resource view.
pub(super) struct SavedPlanSecretCandidates {
    source: SavedPlanSecretCandidateSource,
    provider_inputs: ResolvedProviderInputs,
}

enum SavedPlanSecretCandidateSource {
    Canonical(Box<ExpandedCanonicalResources<InferredExportParam>>),
    #[cfg(test)]
    Parsed(Box<carina_core::parser::InferredFile>),
}

impl SavedPlanSecretCandidates {
    pub(super) fn from_expansion(
        canonical_resources: ExpandedCanonicalResources<InferredExportParam>,
        provider_inputs: ResolvedProviderInputs,
    ) -> Self {
        Self {
            source: SavedPlanSecretCandidateSource::Canonical(Box::new(canonical_resources)),
            provider_inputs,
        }
    }

    #[cfg(test)]
    pub(super) fn from_test_parts(
        parsed: carina_core::parser::InferredFile,
        provider_inputs: ResolvedProviderInputs,
    ) -> Self {
        Self {
            source: SavedPlanSecretCandidateSource::Parsed(Box::new(parsed)),
            provider_inputs,
        }
    }

    pub(super) fn parsed(&self) -> &carina_core::parser::InferredFile {
        match &self.source {
            SavedPlanSecretCandidateSource::Canonical(resources) => resources.parsed(),
            #[cfg(test)]
            SavedPlanSecretCandidateSource::Parsed(parsed) => parsed,
        }
    }

    pub(super) fn provider_inputs(&self) -> &ResolvedProviderInputs {
        &self.provider_inputs
    }
}

impl ReloadedPendingPlanFile {
    /// Reconstruct the minimal plan-time prior-state view needed by
    /// deferred-`for` expansion. Required sources are cloned from snapshots
    /// that already crossed the locked in-flight reload boundary; unrelated
    /// managed snapshots contribute only their refreshed address, avoiding a
    /// deep clone of every saved state.
    pub(super) fn expansion_prior_states(
        &self,
        required_managed: &[Resource],
        required_data_sources: &[DataSource],
    ) -> ExpansionPriorStates {
        let required_managed: HashSet<&ResourceId> = required_managed
            .iter()
            .map(|resource| &resource.id)
            .collect();
        let required_data_sources: HashSet<&ResourceId> = required_data_sources
            .iter()
            .map(|data_source| &data_source.id)
            .collect();
        let mut managed = HashMap::new();
        let mut data_sources = HashMap::new();
        let mut already_refreshed = HashSet::new();
        for entry in self.plan_file.current_states() {
            match entry {
                ReloadedCurrentStateEntry::Managed { id, current } => {
                    if required_managed.contains(id) {
                        managed.insert(id.clone(), current.as_ref().clone());
                    } else if !matches!(current.as_ref(), Current::Absent(_)) {
                        // Plan-time orphan refresh owns these addresses. Keep
                        // only their ids: expansion needs the exclusion set,
                        // not a deep clone of every unrelated saved state.
                        already_refreshed.insert(id.clone());
                    }
                }
                ReloadedCurrentStateEntry::DataSource { id, state } => {
                    if required_data_sources.contains(id) {
                        data_sources.insert(id.clone(), state.as_ref().clone());
                    }
                }
            }
        }
        ExpansionPriorStates {
            managed,
            data_sources,
            already_refreshed,
        }
    }

    pub(super) fn verify_desired(
        self,
        candidates: SavedPlanSecretCandidates,
        apply_target: ApplyTarget<'_>,
    ) -> Result<DesiredPlanFile, AppError> {
        let planned_secret_hashes =
            planned_secret_hashes(&self.plan_file.plan_file().sorted_resources);
        let mut plan_file = self.plan_file;
        super::rehydrate_desired_plan_file(plan_file.plan_file_mut(), &candidates, apply_target)?;
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

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        match self {
            Self::Verified(plan) => plan.current_states(),
            Self::NeedsHistoricalVerification(plan) => plan.current_states(),
        }
    }

    pub(super) fn historical_resource_addresses(&self) -> HistoricalResourceAddresses {
        let plan = match self {
            Self::Verified(plan) => plan.plan(),
            Self::NeedsHistoricalVerification(plan) => &plan.plan_file.plan_file().plan,
        };
        MovedOrigins::from_plan(plan)
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
    plan_file: ReloadedPlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl DesiredVerifiedPlanFile {
    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        &self.plan_file.plan_file().provider_configs
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        &self.plan_file.plan_file().sorted_resources
    }

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan_file.current_states()
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
        let mut redacted_states = HashMap::new();
        for entry in self.plan_file.current_states() {
            match entry {
                ReloadedCurrentStateEntry::Managed { id, current } => match current.as_ref() {
                    carina_core::resource::Current::Absent(state) => {
                        redacted_states.insert(id.clone(), state.state().clone());
                    }
                    carina_core::resource::Current::Present(present) => {
                        redacted_states.insert(id.clone(), present.state().clone());
                    }
                    carina_core::resource::Current::CreateInFlight(_pending) => {}
                    carina_core::resource::Current::MissingIdentifier(state) => {
                        redacted_states.insert(id.clone(), state.state().clone());
                    }
                },
                ReloadedCurrentStateEntry::DataSource { id, state } => {
                    redacted_states.insert(id.clone(), state.as_ref().clone());
                }
            }
        }
        {
            let mut verifier = HistoricalStateVerifier {
                provider,
                actual_states,
                verified_planned_states,
                historical_addresses,
                apply_target,
            };
            for entry in self.plan_file.current_states_mut() {
                match entry {
                    ReloadedCurrentStateEntry::Managed { id, current } => {
                        let verified = match current.as_ref() {
                            carina_core::resource::Current::Absent(state) => {
                                verifier.verify(id, state.state(), None).await?
                            }
                            carina_core::resource::Current::Present(present) => {
                                verifier
                                    .verify(id, present.state(), Some(present.identifier()))
                                    .await?
                            }
                            carina_core::resource::Current::CreateInFlight(_pending) => None,
                            carina_core::resource::Current::MissingIdentifier(state) => {
                                verifier.verify(id, state.state(), None).await?
                            }
                        };
                        if let Some(verified) = verified
                            && let Some(mut state) = current.state_bearing_mut()
                        {
                            *state.attributes_mut() = verified.attributes;
                        }
                    }
                    ReloadedCurrentStateEntry::DataSource { id, state } => {
                        let identifier = state
                            .identifier
                            .as_deref()
                            .and_then(|identifier| AppliedIdentifier::new(identifier).ok());
                        if let Some(verified) =
                            verifier.verify(id, state, identifier.as_ref()).await?
                        {
                            **state = verified;
                        }
                    }
                }
            }
        }
        let rehydrated_plan =
            carina_core::value::rehydrate_plan_historical_secrets_from_verified_states(
                &self.plan_file.plan_file().plan,
                &redacted_states,
                verified_planned_states,
            )
            .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
        self.plan_file.plan_file_mut().plan = rehydrated_plan;

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
    plan_file: ReloadedPlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl RehydratedPlanFile {
    pub(super) fn plan(&self) -> &Plan {
        &self.plan_file.plan_file().plan
    }

    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        &self.plan_file.plan_file().provider_configs
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        &self.plan_file.plan_file().sorted_resources
    }

    pub(super) fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        &self.planned_secret_hashes
    }

    pub(super) fn unresolved_resources(&self) -> &[Resource] {
        &self.plan_file.plan_file().unresolved_resources
    }

    pub(super) fn compositions(&self) -> &[Composition] {
        &self.plan_file.plan_file().compositions
    }

    pub(super) fn data_sources(&self) -> &[DataSource] {
        &self.plan_file.plan_file().data_sources
    }

    pub(super) fn data_source_origins(&self) -> &[DataSource] {
        &self.plan_file.plan_file().data_source_origins
    }

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan_file.current_states()
    }

    pub(super) fn upstream_snapshot(&self) -> &HashMap<String, HashMap<String, Value>> {
        &self.plan_file.plan_file().upstream_snapshot
    }

    pub(super) fn upstream_sources(&self) -> &[UpstreamSource] {
        &self.plan_file.plan_file().upstream_sources
    }

    pub(super) fn wait_bindings(&self) -> &[PlanWaitBinding] {
        &self.plan_file.plan_file().wait_bindings
    }
}
