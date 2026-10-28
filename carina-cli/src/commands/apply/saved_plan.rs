use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::parser::{BackendConfig, InferredExportParam, SavedProviderConfigs};
use carina_core::plan::Plan;
use carina_core::provider::{AppliedIdentifier, PersistedAttributes, Provider, ReadRequest};
use carina_core::resource::{Composition, Current, DataSource, Resource, ResourceId, State, Value};

use super::{ApplyTarget, saved_plan_secret_rehydration_error};
use crate::commands::plan::{
    MovedOrigins, PlanFile, PlanSchemaReferences, PlanWaitBinding, RecordedProviderArtifact,
    ReloadedCurrentStateEntry, ReloadedPlanFile, ResolvedProviderInputs, UpstreamSource,
    recorded_provider_artifacts,
};
use crate::error::{
    AppError, SavedPlanProviderArtifactDifference, SavedPlanProviderArtifactMismatchError,
    SavedPlanSchemaMismatchError,
};
use crate::wiring::{ExpandedCanonicalResources, WiringContext};

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
    schema_references: PlanSchemaReferences,
}

impl InspectedPlanFile {
    pub(super) fn inspect(plan_file: PlanFile) -> Self {
        let schema_references = plan_file.referenced_schema_types();
        if plan_file.contains_redacted_secret_placeholders() {
            Self::NeedsSecretVerification(PendingPlanFile {
                plan_file,
                schema_references,
            })
        } else {
            Self::Verified(VerifiedPlanFile {
                plan_file,
                schema_references,
            })
        }
    }

    pub(super) fn backend_config(&self) -> Option<&BackendConfig> {
        match self {
            Self::Verified(plan) => plan.plan_file.backend_config.as_ref(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.backend_config.as_ref(),
        }
    }

    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        match self {
            Self::Verified(plan) => &plan.plan_file.provider_configs,
            Self::NeedsSecretVerification(plan) => &plan.plan_file.provider_configs,
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
            Self::Verified(VerifiedPlanFile {
                plan_file,
                schema_references,
            }) => plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(|plan_file| {
                    ReloadedInspectedPlanFile::Verified(ReloadedVerifiedPlanFile {
                        plan_file,
                        schema_references,
                    })
                })
                .map_err(AppError::Config),
            Self::NeedsSecretVerification(PendingPlanFile {
                plan_file,
                schema_references,
            }) => plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(|plan_file| {
                    ReloadedInspectedPlanFile::NeedsSecretVerification(ReloadedPendingPlanFile {
                        plan_file,
                        schema_references,
                    })
                })
                .map_err(AppError::Config),
        }
    }
}

/// Saved-plan state after the only locked-state reload transition.
pub(super) enum ReloadedInspectedPlanFile {
    Verified(ReloadedVerifiedPlanFile),
    NeedsSecretVerification(ReloadedPendingPlanFile),
}

/// Placeholder-free saved plan awaiting provider-schema verification.
pub(super) struct ReloadedVerifiedPlanFile {
    plan_file: ReloadedPlanFile,
    schema_references: PlanSchemaReferences,
}

/// The exact provider context whose artifacts and schemas matched a saved plan.
/// Only the verification transition below can construct this witness.
pub(super) struct VerifiedWiringContext(WiringContext);

impl VerifiedWiringContext {
    pub(super) fn context(&self) -> &WiringContext {
        &self.0
    }
}

impl ReloadedInspectedPlanFile {
    /// Check artifact identities and schemas immediately after provider loading.
    /// Every later secret/normalizer/provider transition consumes this proof.
    pub(super) fn verify_loaded_providers(
        self,
        ctx: WiringContext,
        apply_target: ApplyTarget<'_>,
    ) -> Result<(SchemaCheckedPlanFile, VerifiedWiringContext), AppError> {
        let recorded_artifacts = match &self {
            Self::Verified(plan) => plan.plan_file.provider_artifacts(),
            Self::NeedsSecretVerification(plan) => plan.plan_file.provider_artifacts(),
        };
        let loaded_artifacts = recorded_provider_artifacts(&ctx);
        let provider_names = recorded_artifacts
            .keys()
            .chain(loaded_artifacts.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let providers = provider_names
            .into_iter()
            .filter_map(|provider| {
                let recorded = recorded_artifacts.get(&provider);
                let loaded = loaded_artifacts.get(&provider);
                if recorded == loaded {
                    None
                } else {
                    Some((
                        provider,
                        SavedPlanProviderArtifactDifference {
                            recorded: recorded.map(RecordedProviderArtifact::summary),
                            loaded: loaded.map(RecordedProviderArtifact::summary),
                        },
                    ))
                }
            })
            .collect::<BTreeMap<_, _>>();
        if !providers.is_empty() {
            return Err(AppError::SavedPlanProviderArtifactMismatch(
                SavedPlanProviderArtifactMismatchError {
                    providers,
                    replan_command: apply_target.replan_command().to_string(),
                },
            ));
        }

        let map_mismatch = |mismatch: crate::commands::plan::PlanSchemaFingerprintMismatch| {
            AppError::SavedPlanSchemaMismatch(SavedPlanSchemaMismatchError {
                providers: mismatch.providers,
                replan_command: apply_target.replan_command().to_string(),
            })
        };
        let checked_plan = match self {
            Self::Verified(plan) => {
                plan.plan_file
                    .verify_schema_fingerprints(&plan.schema_references, ctx.schemas())
                    .map_err(map_mismatch)?;
                SchemaCheckedPlanFile::Verified(RehydratedPlanFile {
                    plan_file: plan.plan_file,
                    planned_secret_hashes: HashMap::new(),
                })
            }
            Self::NeedsSecretVerification(plan) => {
                plan.plan_file
                    .verify_schema_fingerprints(&plan.schema_references, ctx.schemas())
                    .map_err(map_mismatch)?;
                SchemaCheckedPlanFile::NeedsSecretVerification(SchemaCheckedPendingPlanFile {
                    plan_file: plan.plan_file,
                })
            }
        };
        Ok((checked_plan, VerifiedWiringContext(ctx)))
    }
}

/// Deserialized saved plan whose typed placeholders still need fresh desired
/// values. There is intentionally no accessor for the inner plan.
pub(super) struct PendingPlanFile {
    plan_file: PlanFile,
    schema_references: PlanSchemaReferences,
}

/// Reloaded saved plan whose desired secret placeholders still need fresh
/// source values.
pub(super) struct ReloadedPendingPlanFile {
    plan_file: ReloadedPlanFile,
    schema_references: PlanSchemaReferences,
}

pub(super) struct ExpansionPriorStates {
    pub managed: HashMap<ResourceId, Current>,
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

/// Reloaded saved plan proven to match the currently loaded schemas. The
/// secret-verification transition accepts only this checked state.
pub(super) enum SchemaCheckedPlanFile {
    Verified(RehydratedPlanFile),
    NeedsSecretVerification(SchemaCheckedPendingPlanFile),
}

/// Schema-checked saved plan whose desired secret placeholders still need
/// verification against the current configuration.
pub(super) struct SchemaCheckedPendingPlanFile {
    plan_file: ReloadedPlanFile,
}

impl SchemaCheckedPendingPlanFile {
    /// Reconstruct the minimal plan-time prior-state view needed by
    /// deferred-`for` expansion. Required sources are cloned from snapshots
    /// that already crossed the locked in-flight reload boundary; unrelated
    /// managed snapshots contribute only their refreshed address, avoiding a
    /// deep clone of every saved state.
    pub(super) fn expansion_prior_states(
        &self,
        required_managed: &[Resource],
    ) -> ExpansionPriorStates {
        let required_managed: HashSet<&ResourceId> = required_managed
            .iter()
            .map(|resource| &resource.id)
            .collect();
        let mut managed = HashMap::new();
        let mut already_refreshed = HashSet::new();
        for entry in self.plan_file.current_states() {
            let ReloadedCurrentStateEntry::Managed { id, current } = entry;
            if required_managed.contains(id) {
                managed.insert(id.clone(), current.as_ref().clone());
            } else if !matches!(current.as_ref(), Current::Absent(_)) {
                // Plan-time orphan refresh owns these addresses. Keep only
                // their ids: expansion needs the exclusion set, not a deep
                // clone of every unrelated saved state.
                already_refreshed.insert(id.clone());
            }
        }
        ExpansionPriorStates {
            managed,
            already_refreshed,
        }
    }

    pub(super) fn data_source_values(&self) -> Result<DataSourceValues, AppError> {
        super::data_source_values_from_saved_plan(
            self.plan_file.data_sources(),
            self.plan_file.data_source_values(),
        )
    }
    pub(super) fn verify_desired(
        self,
        candidates: SavedPlanSecretCandidates,
        apply_target: ApplyTarget<'_>,
    ) -> Result<DesiredPlanFile, AppError> {
        let planned_secret_hashes = planned_secret_hashes(self.plan_file.sorted_resources());
        let mut plan_file = self.plan_file;
        super::rehydrate_desired_plan_file(&mut plan_file, &candidates, apply_target)?;
        Ok(DesiredPlanFile::NeedsHistoricalVerification(
            DesiredVerifiedPlanFile {
                plan_file,
                planned_secret_hashes,
            },
        ))
    }
}

/// Common post-schema-check and desired-verification state. Historical
/// placeholders may still require provider reads before execution.
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
            Self::NeedsHistoricalVerification(plan) => plan.plan_file.plan(),
        };
        MovedOrigins::from_plan(plan)
    }

    /// Complete historical secret verification and mint the sole executable
    /// saved-plan capability while retaining the schema-check proof.
    pub(super) async fn verify_historical(
        self,
        provider: &dyn Provider,
        actual_states: &mut HashMap<ResourceId, State>,
        verified_planned_states: &mut HashMap<ResourceId, State>,
        historical_addresses: &HistoricalResourceAddresses,
        apply_target: ApplyTarget<'_>,
    ) -> Result<ExecutablePlanFile, AppError> {
        let plan = match self {
            Self::Verified(plan) => plan,
            Self::NeedsHistoricalVerification(plan) => {
                plan.verify_historical(
                    provider,
                    actual_states,
                    verified_planned_states,
                    historical_addresses,
                    apply_target,
                )
                .await?
            }
        };
        Ok(ExecutablePlanFile { plan })
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
        self.plan_file.provider_configs()
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        self.plan_file.sorted_resources()
    }

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan_file.current_states()
    }

    /// Verify every historical placeholder against a fresh provider read and
    /// return the rehydrated payload to the outer executor-gate transition.
    async fn verify_historical(
        mut self,
        provider: &dyn Provider,
        actual_states: &mut HashMap<ResourceId, State>,
        verified_planned_states: &mut HashMap<ResourceId, State>,
        historical_addresses: &HistoricalResourceAddresses,
        apply_target: ApplyTarget<'_>,
    ) -> Result<RehydratedPlanFile, AppError> {
        let mut redacted_states = HashMap::new();
        for entry in self.plan_file.current_states() {
            let ReloadedCurrentStateEntry::Managed { id, current } = entry;
            match current.as_ref() {
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
            }
        }
        for saved in self.plan_file.data_source_values() {
            if let DataSourceValue::Read(state) = &saved.value {
                redacted_states.insert(saved.id.clone(), state.as_ref().clone());
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
                let ReloadedCurrentStateEntry::Managed { id, current } = entry;
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
            for saved in self.plan_file.data_source_values_mut() {
                if let DataSourceValue::Read(state) = &mut saved.value {
                    let identifier = state
                        .identifier
                        .as_deref()
                        .and_then(|identifier| AppliedIdentifier::new(identifier).ok());
                    if let Some(verified) = verifier
                        .verify(&saved.id, state, identifier.as_ref())
                        .await?
                    {
                        **state = verified;
                    }
                }
            }
        }
        let rehydrated_plan =
            carina_core::value::rehydrate_plan_historical_secrets_from_verified_states(
                self.plan_file.plan(),
                &redacted_states,
                verified_planned_states,
            )
            .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
        *self.plan_file.plan_mut() = rehydrated_plan;

        Ok(RehydratedPlanFile {
            plan_file: self.plan_file,
            planned_secret_hashes: self.planned_secret_hashes,
        })
    }
}

/// A saved plan that has crossed both typed secret-verification boundaries.
///
/// This is still not executable: the schema-checked [`DesiredPlanFile`]
/// transition is the only path that can wrap it in [`ExecutablePlanFile`].
pub(super) struct RehydratedPlanFile {
    plan_file: ReloadedPlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl RehydratedPlanFile {
    fn plan(&self) -> &Plan {
        self.plan_file.plan()
    }

    fn provider_configs(&self) -> &SavedProviderConfigs {
        self.plan_file.provider_configs()
    }

    fn sorted_resources(&self) -> &[Resource] {
        self.plan_file.sorted_resources()
    }

    fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        &self.planned_secret_hashes
    }

    fn unresolved_resources(&self) -> &[Resource] {
        self.plan_file.unresolved_resources()
    }

    fn compositions(&self) -> &[Composition] {
        self.plan_file.compositions()
    }

    fn data_sources(&self) -> &[DataSource] {
        self.plan_file.data_sources()
    }

    fn data_source_origins(&self) -> &[DataSource] {
        self.plan_file.data_source_origins()
    }

    fn data_source_values(&self) -> &[crate::commands::plan::SavedDataSourceValue] {
        self.plan_file.data_source_values()
    }

    fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan_file.current_states()
    }

    fn upstream_snapshot(&self) -> &HashMap<String, HashMap<String, Value>> {
        self.plan_file.upstream_snapshot()
    }

    fn upstream_sources(&self) -> &[UpstreamSource] {
        self.plan_file.upstream_sources()
    }

    fn wait_bindings(&self) -> &[PlanWaitBinding] {
        self.plan_file.wait_bindings()
    }
}

/// The only saved-plan type accepted by the executor gateway. Construction is
/// private to [`DesiredPlanFile::verify_historical`], so a deserialized
/// plan cannot execute without the provider-schema comparison.
pub(super) struct ExecutablePlanFile {
    plan: RehydratedPlanFile,
}

impl ExecutablePlanFile {
    pub(super) fn plan(&self) -> &Plan {
        self.plan.plan()
    }

    pub(super) fn sorted_resources(&self) -> &[Resource] {
        self.plan.sorted_resources()
    }

    pub(super) fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        self.plan.planned_secret_hashes()
    }

    pub(super) fn unresolved_resources(&self) -> &[Resource] {
        self.plan.unresolved_resources()
    }

    pub(super) fn compositions(&self) -> &[Composition] {
        self.plan.compositions()
    }

    pub(super) fn data_sources(&self) -> &[DataSource] {
        self.plan.data_sources()
    }

    pub(super) fn data_source_origins(&self) -> &[DataSource] {
        self.plan.data_source_origins()
    }

    pub(super) fn data_source_values(&self) -> &[crate::commands::plan::SavedDataSourceValue] {
        self.plan.data_source_values()
    }

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan.current_states()
    }

    pub(super) fn upstream_snapshot(&self) -> &HashMap<String, HashMap<String, Value>> {
        self.plan.upstream_snapshot()
    }

    pub(super) fn upstream_sources(&self) -> &[UpstreamSource] {
        self.plan.upstream_sources()
    }

    pub(super) fn wait_bindings(&self) -> &[PlanWaitBinding] {
        self.plan.wait_bindings()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use carina_core::data_source_value::DataSourceValue;
    use carina_core::parser::ProviderConfig;
    use carina_core::plan::Plan;
    use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
    use carina_core::resource::{ConcreteValue, DataSource, DeferredValue, State, Value};
    use indexmap::IndexMap;

    use super::{
        DesiredVerifiedPlanFile, HistoricalResourceAddresses, InspectedPlanFile,
        SchemaCheckedPlanFile,
    };
    use crate::commands::apply::ApplyTarget;
    use crate::commands::plan::{
        PlanFile, PlanFileContents, RecordedProviderArtifact, SavedDataSourceValue,
    };
    use crate::error::AppError;
    use crate::wiring::{LoadedProviderFactory, ProviderLoad, WiringContext};

    struct ArtifactFactory {
        name: &'static str,
    }

    impl ProviderFactory for ArtifactFactory {
        fn name(&self) -> &str {
            self.name
        }

        fn display_name(&self) -> &str {
            "Saved-plan artifact test provider"
        }

        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            Default::default()
        }

        fn validate_config(
            &self,
            _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            "test-region".to_string()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _config: &carina_core::provider::ProviderReadyConfig,
        ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
            Box::pin(async {
                Ok(Box::new(carina_provider_mock::MockProvider::new()) as Box<dyn Provider>)
            })
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    fn provider_config(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            source: Some(format!("file://{name}.wasm").parse().unwrap()),
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
        }
    }

    fn remote_provider_config(name: &str, source: &str) -> ProviderConfig {
        ProviderConfig {
            source: Some(source.parse().unwrap()),
            ..provider_config(name)
        }
    }

    fn wasm_context(name: &'static str, bytes: &[u8]) -> WiringContext {
        ProviderLoad::for_test(
            vec![LoadedProviderFactory::for_test_with_bytes(
                Box::new(ArtifactFactory { name }),
                bytes,
            )],
            &[provider_config(name)],
        )
        .into_complete_context()
        .expect("test provider load must be complete")
    }

    fn builtin_context() -> WiringContext {
        WiringContext::new_for_test(Vec::new(), &[])
    }

    fn saved_provider_configs(
        configs: Vec<ProviderConfig>,
    ) -> carina_core::parser::SavedProviderConfigs {
        let parsed = carina_core::parser::InferredFile {
            providers: configs,
            ..Default::default()
        };
        let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("test provider configs must resolve");
        carina_core::parser::SavedProviderConfigs::from(&resolved)
    }

    fn empty_plan_contents(provider_configs: Vec<ProviderConfig>) -> PlanFileContents {
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-10-28T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(provider_configs),
            backend_config: None,
            plan: Plan::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        }
    }

    fn plan_recorded_with(ctx: &WiringContext, provider_configs: Vec<ProviderConfig>) -> PlanFile {
        empty_plan_contents(provider_configs)
            .record(ctx)
            .expect("test plan provider evidence must be valid")
    }

    fn apply_target() -> ApplyTarget<'static> {
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("."),
        }
    }

    fn reloaded(plan: PlanFile) -> super::ReloadedInspectedPlanFile {
        InspectedPlanFile::inspect(plan)
            .reload_in_flight_from_locked_state(None)
            .expect("reload saved plan")
    }

    #[test]
    fn changed_artifact_with_identical_schemas_fails_before_schema_checked_state() {
        let planned = wasm_context("mock", b"artifact A");
        let loaded = wasm_context("mock", b"artifact B");

        let result = reloaded(plan_recorded_with(&planned, vec![provider_config("mock")]))
            .verify_loaded_providers(loaded, apply_target());

        let Err(AppError::SavedPlanProviderArtifactMismatch(mismatch)) = result else {
            panic!("changed provider bytes must not produce SchemaCheckedPlanFile")
        };
        assert_eq!(mismatch.providers.keys().collect::<Vec<_>>(), vec!["mock"]);
    }

    #[test]
    fn record_rejects_artifact_origin_that_does_not_match_config_source() {
        let context = wasm_context("mock", b"artifact");
        let error = empty_plan_contents(vec![remote_provider_config("mock", "carina-rs/mock")])
            .record(&context)
            .expect_err("a file-origin load cannot evidence a remote provider source")
            .to_string();

        assert!(error.contains("mock"), "provider is missing: {error}");
        assert!(
            error.contains("carina-rs/mock"),
            "expected source is missing: {error}"
        );
        assert!(
            error.contains("file"),
            "recorded origin is missing: {error}"
        );
    }

    #[test]
    fn decode_rejects_locked_artifact_source_that_does_not_match_config_source() {
        let context = wasm_context("mock", b"artifact");
        let plan = plan_recorded_with(&context, vec![provider_config("mock")]);
        let mut encoded = serde_json::to_value(plan).expect("saved plan serializes");
        encoded["provider_configs"][0]["source"] = serde_json::json!("carina-rs/mock");
        encoded["provider_artifacts"]["mock"]["origin"] = serde_json::json!({
            "kind": "locked",
            "source": "other/mock",
            "version": {
                "kind": "version",
                "version": "1.0.0"
            }
        });

        let error = serde_json::from_value::<PlanFile>(encoded)
            .expect_err("decode must bind a locked artifact to the saved remote source")
            .to_string();

        assert!(
            error.contains("carina-rs/mock"),
            "expected source is missing: {error}"
        );
        assert!(
            error.contains("other/mock"),
            "recorded source is missing: {error}"
        );
    }

    #[test]
    fn valid_file_artifact_origin_round_trips() {
        let context = wasm_context("mock", b"artifact");
        let plan = plan_recorded_with(&context, vec![provider_config("mock")]);
        let encoded = serde_json::to_vec(&plan).expect("saved plan serializes");
        let decoded =
            serde_json::from_slice::<PlanFile>(&encoded).expect("matching file origin must decode");

        assert_eq!(decoded.provider_artifacts(), plan.provider_artifacts());
    }

    #[test]
    fn unchanged_artifact_and_schemas_produce_schema_checked_state() {
        let planned = wasm_context("mock", b"artifact A");
        let loaded = wasm_context("mock", b"artifact A");

        let result = reloaded(plan_recorded_with(&planned, vec![provider_config("mock")]))
            .verify_loaded_providers(loaded, apply_target());

        assert!(matches!(
            result,
            Ok((SchemaCheckedPlanFile::Verified(_), _))
        ));
    }

    #[test]
    fn builtin_artifact_records_and_verifies_as_builtin() {
        let planned = builtin_context();
        let plan = plan_recorded_with(&planned, Vec::new());
        assert_eq!(
            plan.provider_artifacts().get("mock"),
            Some(&RecordedProviderArtifact::BuiltIn)
        );

        let result = reloaded(plan).verify_loaded_providers(builtin_context(), apply_target());

        assert!(matches!(
            result,
            Ok((SchemaCheckedPlanFile::Verified(_), _))
        ));
    }

    #[test]
    fn builtin_vs_wasm_and_missing_or_extra_provider_keys_are_mismatches() {
        let builtin_plan = plan_recorded_with(&builtin_context(), Vec::new());

        let same_name = reloaded(builtin_plan.clone())
            .verify_loaded_providers(wasm_context("mock", b"artifact"), apply_target());
        assert!(matches!(
            same_name,
            Err(AppError::SavedPlanProviderArtifactMismatch(ref mismatch))
                if mismatch.providers.contains_key("mock")
        ));

        let changed_keys = reloaded(builtin_plan)
            .verify_loaded_providers(wasm_context("other", b"artifact"), apply_target());
        let Err(AppError::SavedPlanProviderArtifactMismatch(mismatch)) = changed_keys else {
            panic!("missing and extra provider keys must invalidate a saved plan")
        };
        assert_eq!(
            mismatch.providers.keys().collect::<Vec<_>>(),
            vec!["mock", "other"]
        );
    }

    #[tokio::test]
    async fn historical_verification_rehydrates_read_outcome_without_current_state_row() {
        let data_source =
            DataSource::with_provider("mock", "iam.Roles", "roles", None).with_binding("roles");
        let plaintext = Value::Concrete(ConcreteValue::String("provider-secret".into()));
        let state_with_secret = State::existing(
            data_source.id.clone(),
            HashMap::from([(
                "token".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(plaintext.clone()))),
            )]),
        );
        let redacted = carina_core::value::redact_secrets_in_state(&state_with_secret)
            .expect("redact data-source output");
        let actual = State::existing(
            data_source.id.clone(),
            HashMap::from([("token".to_string(), plaintext.clone())]),
        );
        let plan = Plan::new();
        let plan_file = PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-10-28T00:00:00Z".to_string(),
            source_path: "main.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: serde_json::from_value(serde_json::json!([]))
                .expect("empty saved provider configs"),
            backend_config: None,
            plan: plan.clone(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: vec![data_source.clone()],
            data_source_origins: Vec::new(),
            data_source_values: vec![SavedDataSourceValue {
                id: data_source.id.clone(),
                value: DataSourceValue::read(redacted),
            }],
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        }
        .record(&builtin_context())
        .expect("test plan provider evidence must be valid")
        .reload_in_flight_from_locked_state(None)
        .expect("reload saved plan");
        let pending = DesiredVerifiedPlanFile {
            plan_file,
            planned_secret_hashes: HashMap::new(),
        };
        let mut actual_states = HashMap::from([(data_source.id.clone(), actual)]);
        let mut verified_planned_states = HashMap::new();

        let rehydrated = pending
            .verify_historical(
                &carina_provider_mock::MockProvider::new(),
                &mut actual_states,
                &mut verified_planned_states,
                &HistoricalResourceAddresses::from_plan(&plan),
                ApplyTarget::SavedPlan {
                    plan_file: Path::new("saved.plan"),
                    source_dir: Path::new("."),
                },
            )
            .await
            .expect("verify data-source historical secret");

        let DataSourceValue::Read(verified) = &rehydrated.data_source_values()[0].value else {
            panic!("read outcome changed variants")
        };
        assert_eq!(verified.attributes.get("token"), Some(&plaintext));
    }
}
