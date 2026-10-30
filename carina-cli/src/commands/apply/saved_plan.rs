use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::differ::{Orphan, Orphans, restore_current_move_source};
use carina_core::parser::{BackendConfig, InferredExportParam, SavedProviderConfigs};
use carina_core::plan::SavedPlan;
use carina_core::provider::{AppliedIdentifier, PersistedAttributes, Provider, ReadRequest};
use carina_core::resource::{
    Current, DataSource, ResolvedResourceId, Resource, ResourceId, SavedComposition,
    SavedDataSource, SavedResource, State, Value,
};
use carina_state::{ResourceState, StateFile};

use super::{ApplyTarget, saved_plan_secret_rehydration_error};
use crate::commands::plan::{
    MovedOrigins, PlanFile, PlanSchemaReferences, PlanWaitBinding, RecordedProviderArtifact,
    ReloadedCurrentStateEntry, ReloadedPlanFile, ResolvedProviderInputs,
    UnresolvedReloadedPlanFile, UpstreamSource, recorded_provider_artifacts,
};
use crate::error::{
    AppError, SavedPlanProviderArtifactDifference, SavedPlanProviderArtifactMismatchError,
    SavedPlanSchemaMismatchError,
};
use crate::wiring::{
    CheckedSavedPlanInputs, CompleteProviderLoad, ExpandedCanonicalResources, SavedPlanReplayMoves,
    ValidatedConfig, WiringContext,
};

pub(super) type PlannedSecretHashes = HashMap<ResourceId, PersistedAttributes>;

fn planned_secret_hashes(resources: &[SavedResource]) -> PlannedSecretHashes {
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

    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        match self {
            Self::Verified(plan) => &plan.plan_file.provider_configs,
            Self::NeedsSecretVerification(plan) => &plan.plan_file.provider_configs,
        }
    }
}

/// Inspected saved plan bound to the provider-load state required by its
/// secret shape. The mismatched states (a secret plan with an unchecked load,
/// or a placeholder-free plan with checked current-config inputs) have no
/// representation.
pub(super) enum ProviderBoundPlanFile {
    Verified {
        plan: VerifiedPlanFile,
        providers: CompleteProviderLoad,
    },
    NeedsSecretVerification {
        plan: PendingPlanFile,
        providers: CheckedSavedPlanInputs,
    },
}

impl ProviderBoundPlanFile {
    pub(super) fn verified(plan: VerifiedPlanFile, providers: CompleteProviderLoad) -> Self {
        Self::Verified { plan, providers }
    }

    pub(super) fn needs_secret_verification(
        plan: PendingPlanFile,
        providers: CheckedSavedPlanInputs,
    ) -> Self {
        Self::NeedsSecretVerification { plan, providers }
    }

    pub(super) fn backend_config(&self) -> Option<&BackendConfig> {
        match self {
            Self::Verified { plan, .. } => plan.plan_file.backend_config.as_ref(),
            Self::NeedsSecretVerification { plan, .. } => plan.plan_file.backend_config.as_ref(),
        }
    }

    pub(super) fn state_lineage(&self) -> Option<&str> {
        match self {
            Self::Verified { plan, .. } => plan.plan_file.state_lineage.as_deref(),
            Self::NeedsSecretVerification { plan, .. } => plan.plan_file.state_lineage.as_deref(),
        }
    }

    pub(super) fn state_serial(&self) -> Option<u64> {
        match self {
            Self::Verified { plan, .. } => plan.plan_file.state_serial,
            Self::NeedsSecretVerification { plan, .. } => plan.plan_file.state_serial,
        }
    }

    pub(super) fn data_source_origins(&self) -> &[SavedDataSource] {
        match self {
            Self::Verified { plan, .. } => &plan.plan_file.data_source_origins,
            Self::NeedsSecretVerification { plan, .. } => &plan.plan_file.data_source_origins,
        }
    }

    pub(super) fn reload_in_flight_from_locked_state(
        self,
        state_file: Option<&carina_state::StateFile>,
    ) -> Result<ReloadedProviderBoundPlanFile, AppError> {
        match self {
            Self::Verified {
                plan:
                    VerifiedPlanFile {
                        plan_file,
                        schema_references,
                    },
                providers,
            } => plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(|plan_file| ReloadedProviderBoundPlanFile::Verified {
                    plan: ReloadedVerifiedPlanFile {
                        plan_file,
                        schema_references,
                    },
                    providers,
                })
                .map_err(AppError::Config),
            Self::NeedsSecretVerification {
                plan:
                    PendingPlanFile {
                        plan_file,
                        schema_references,
                    },
                providers,
            } => plan_file
                .reload_in_flight_from_locked_state(state_file)
                .map(
                    |plan_file| ReloadedProviderBoundPlanFile::NeedsSecretVerification {
                        plan: ReloadedPendingPlanFile {
                            plan_file,
                            schema_references,
                        },
                        providers,
                    },
                )
                .map_err(AppError::Config),
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(
        plan: InspectedPlanFile,
        providers: CompleteProviderLoad,
        base_dir: &std::path::Path,
    ) -> Result<Self, AppError> {
        match plan {
            InspectedPlanFile::Verified(plan) => Ok(Self::verified(plan, providers)),
            InspectedPlanFile::NeedsSecretVerification(plan) => {
                let unresolved = carina_core::parser::ParsedFile::default();
                let providers = providers.check_saved_plan_config_pair(
                    carina_core::parser::InferredFile::default(),
                    carina_core::config_loader::UnresolvedConfigSnapshot::from_parsed(unresolved),
                    base_dir,
                    false,
                    &[],
                )?;
                Ok(Self::needs_secret_verification(plan, providers))
            }
        }
    }
}

/// Provider-bound saved-plan state after the only locked-state reload.
pub(super) enum ReloadedProviderBoundPlanFile {
    Verified {
        plan: ReloadedVerifiedPlanFile,
        providers: CompleteProviderLoad,
    },
    NeedsSecretVerification {
        plan: ReloadedPendingPlanFile,
        providers: CheckedSavedPlanInputs,
    },
}

/// Placeholder-free saved plan awaiting provider-schema verification.
pub(super) struct ReloadedVerifiedPlanFile {
    plan_file: UnresolvedReloadedPlanFile,
    schema_references: PlanSchemaReferences,
}

/// Proof that one saved plan's artifact identities and schemas were checked
/// against the borrowed provider context.
///
/// Its constructor is private to this module. Wiring can consume the proof,
/// but no other caller can manufacture a successful saved-plan transition.
pub(crate) struct SavedPlanArtifactsChecked<'context, T> {
    plan: T,
    context: std::marker::PhantomData<&'context WiringContext>,
}

impl<'context, T> SavedPlanArtifactsChecked<'context, T> {
    fn new(plan: T, _context: &'context WiringContext) -> Self {
        Self {
            plan,
            context: std::marker::PhantomData,
        }
    }

    pub(crate) fn into_plan(self) -> T {
        self.plan
    }
}

impl ReloadedVerifiedPlanFile {
    /// Consume a complete provider load and return the checked plan together
    /// with the only wiring value that can execute it.
    pub(super) fn verify_provider_load(
        self,
        providers: CompleteProviderLoad,
        apply_target: ApplyTarget<'_>,
    ) -> Result<ValidatedDesiredPlan, AppError> {
        let Self {
            plan_file,
            schema_references,
        } = self;
        let (plan, config) = providers.verify_saved_plan_artifacts(move |ctx| {
            check_loaded_providers(&plan_file, &schema_references, ctx, apply_target)?;
            let plan_file = plan_file.resolve_with_schemas(ctx.schemas())?;
            Ok(SavedPlanArtifactsChecked::new(
                RehydratedPlanFile {
                    plan_file,
                    planned_secret_hashes: HashMap::new(),
                },
                ctx,
            ))
        })?;
        Ok(ValidatedDesiredPlan::without_current_config(plan, config))
    }
}

impl ReloadedPendingPlanFile {
    /// Verify artifacts against the complete load that already checked the
    /// current config, and return its retained canonical pair with the checked
    /// plan and executable wiring.
    pub(super) fn verify_provider_load(
        self,
        providers: CheckedSavedPlanInputs,
        apply_target: ApplyTarget<'_>,
    ) -> Result<
        (
            SchemaCheckedPendingPlanFile,
            ValidatedConfig<crate::wiring::CanonicalResourcePair<InferredExportParam>>,
        ),
        AppError,
    > {
        let Self {
            plan_file,
            schema_references,
        } = self;
        providers.verify_saved_plan_artifacts(move |ctx| {
            check_loaded_providers(&plan_file, &schema_references, ctx, apply_target)?;
            let plan_file = plan_file.resolve_with_schemas(ctx.schemas())?;
            Ok(SavedPlanArtifactsChecked::new(
                SchemaCheckedPendingPlanFile { plan_file },
                ctx,
            ))
        })
    }
}

/// Check artifact identities and schemas against the context borrowed by the
/// consuming wiring transition.
fn check_loaded_providers(
    plan_file: &UnresolvedReloadedPlanFile,
    schema_references: &PlanSchemaReferences,
    ctx: &WiringContext,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    let recorded_artifacts = plan_file.provider_artifacts();
    let loaded_artifacts = recorded_provider_artifacts(ctx);
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

    plan_file
        .verify_schema_fingerprints(schema_references, ctx.schemas())
        .map_err(
            |mismatch: crate::commands::plan::PlanSchemaFingerprintMismatch| {
                AppError::SavedPlanSchemaMismatch(SavedPlanSchemaMismatchError {
                    providers: mismatch.providers,
                    replan_command: apply_target.replan_command().to_string(),
                })
            },
        )
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
    plan_file: UnresolvedReloadedPlanFile,
    schema_references: PlanSchemaReferences,
}

pub(super) struct ExpansionPriorStates {
    pub managed: HashMap<ResourceId, Current>,
    pub orphans: Orphans,
    pub already_refreshed: HashSet<ResourceId>,
}

fn saved_orphan_from_current(current: Current, row: &ResourceState) -> Orphan {
    match current {
        Current::Absent(state) => {
            Orphan::absent(state, row.binding.clone(), row.directives.clone().into())
        }
        Current::Present(present) => {
            Orphan::present(present, row.binding.clone(), row.directives.clone().into())
        }
        Current::CreateInFlight(pending) => {
            Orphan::create_in_flight(pending, row.binding.clone(), row.directives.clone().into())
        }
        Current::MissingIdentifier(state) => {
            Orphan::missing_identifier(state, row.binding.clone(), row.directives.clone().into())
        }
    }
}

/// Fresh source values whose managed-resource slice is proven to have crossed
/// the same deferred-`for` expansion seam as plan recording.
///
/// The fields are private and the production constructor consumes the opaque
/// [`ExpandedCanonicalResources`] witness, so a pending saved plan cannot be
/// verified accidentally against the parser's pre-expansion resource view.
pub(super) struct SavedPlanSecretCandidates {
    validated: ValidatedConfig<ExpandedCanonicalResources<InferredExportParam>>,
    provider_inputs: ResolvedProviderInputs,
}

#[cfg(test)]
pub(super) struct TestSavedPlanSecretCandidates {
    parsed: carina_core::parser::ModuleResolvedInferredFile,
    provider_inputs: ResolvedProviderInputs,
}

#[cfg(test)]
impl TestSavedPlanSecretCandidates {
    pub(super) fn parsed(&self) -> &carina_core::parser::ModuleResolvedInferredFile {
        &self.parsed
    }

    pub(super) fn provider_inputs(&self) -> &ResolvedProviderInputs {
        &self.provider_inputs
    }
}

impl SavedPlanSecretCandidates {
    pub(super) fn from_expansion(
        validated: ValidatedConfig<ExpandedCanonicalResources<InferredExportParam>>,
        provider_inputs: ResolvedProviderInputs,
    ) -> Self {
        Self {
            validated,
            provider_inputs,
        }
    }

    #[cfg(test)]
    pub(super) fn from_test_parts(
        parsed: carina_core::parser::ModuleResolvedInferredFile,
        provider_inputs: ResolvedProviderInputs,
    ) -> TestSavedPlanSecretCandidates {
        TestSavedPlanSecretCandidates {
            parsed,
            provider_inputs,
        }
    }

    pub(super) fn parsed(&self) -> &carina_core::parser::ModuleResolvedInferredFile {
        self.validated.resources().parsed()
    }

    pub(super) fn provider_inputs(&self) -> &ResolvedProviderInputs {
        &self.provider_inputs
    }
}

/// Schema-checked saved plan whose desired secret placeholders still need
/// verification against the current configuration.
pub(crate) struct SchemaCheckedPendingPlanFile {
    plan_file: ReloadedPlanFile,
}

impl SchemaCheckedPendingPlanFile {
    /// Reconstruct the minimal plan-time prior-state view needed by
    /// deferred-`for` expansion. Saved snapshots have already crossed address
    /// reconciliation, so move targets are restored through their plan-minted
    /// move authority before the current configuration replays those moves.
    /// Unrelated managed snapshots contribute only their refreshed address,
    /// avoiding a deep clone of every saved state.
    pub(super) fn expansion_prior_states(
        &self,
        required_managed: &[Resource],
        replay_moves: &SavedPlanReplayMoves,
        state_file: &Option<StateFile>,
    ) -> Result<ExpansionPriorStates, AppError> {
        let required_managed: HashSet<&ResourceId> = required_managed
            .iter()
            .map(|resource| &resource.id)
            .collect();
        let plan = self.plan_file.plan();
        let moved_origins = MovedOrigins::from_plan(plan);
        let mut managed = HashMap::new();
        let mut orphans = Orphans::new();
        let mut already_refreshed = HashSet::new();
        for entry in self.plan_file.current_states() {
            let ReloadedCurrentStateEntry::Managed { id, current } = entry;
            let plan_origin = moved_origins.origin_for(id);
            let replay_origin = if plan_origin != id && replay_moves.replays_origin(plan_origin) {
                plan_origin
            } else {
                id
            };
            if !required_managed.contains(id) && !matches!(current.as_ref(), Current::Absent(_)) {
                // A post-expansion orphan was refreshed before plan recording.
                // Replaying its move from the restored origin must not make the
                // child refresh pass read the moved-to address a second time.
                already_refreshed.insert(id.clone());
            }
            if required_managed.contains(id) || plan_origin != id {
                let current = if replay_origin == id {
                    current.as_ref().clone()
                } else {
                    let authority = plan.move_authority(replay_origin, id).ok_or_else(|| {
                        AppError::Validation(format!(
                            "saved plan has no move authority from {replay_origin} to {id}"
                        ))
                    })?;
                    restore_current_move_source(current.as_ref().clone(), &authority)?
                };
                if replay_origin != id && replay_moves.replays_as_explicit_move(replay_origin) {
                    let row = state_file
                        .as_ref()
                        .and_then(|state_file| state_file.find_resource_for_id(replay_origin))
                        .ok_or_else(|| AppError::MoveSourceNotLoaded {
                            from: Box::new(ResolvedResourceId::new(replay_origin.clone())),
                            to: Box::new(ResolvedResourceId::new(id.clone())),
                        })?;
                    orphans.insert(saved_orphan_from_current(current, row));
                } else {
                    managed.insert(replay_origin.clone(), current);
                }
            } else if !matches!(current.as_ref(), Current::Absent(_)) {
                // Plan-time orphan refresh owns these addresses. Keep only
                // their ids: expansion needs the exclusion set, not a deep
                // clone of every unrelated saved state.
                already_refreshed.insert(id.clone());
            }
        }
        for id in required_managed {
            managed
                .entry((*id).clone())
                .or_insert_with(|| Current::absent((*id).clone()));
        }
        Ok(ExpansionPriorStates {
            managed,
            orphans,
            already_refreshed,
        })
    }

    pub(super) fn data_source_values(
        &self,
        data_sources: &[DataSource],
    ) -> Result<DataSourceValues, AppError> {
        super::data_source_values_from_saved_plan(data_sources, self.plan_file.data_source_values())
    }
    pub(super) fn verify_desired(
        self,
        candidates: SavedPlanSecretCandidates,
        apply_target: ApplyTarget<'_>,
    ) -> Result<ValidatedDesiredPlan, AppError> {
        let planned_secret_hashes = planned_secret_hashes(self.plan_file.sorted_resources());
        let mut plan_file = self.plan_file;
        super::rehydrate_desired_plan_file(&mut plan_file, &candidates, apply_target)?;
        Ok(ValidatedDesiredPlan::with_current_config(
            DesiredPlanFile::NeedsHistoricalVerification(DesiredVerifiedPlanFile {
                plan_file,
                planned_secret_hashes,
            }),
            candidates.validated,
        ))
    }
}

/// Common post-schema-check and desired-verification state. Historical
/// placeholders may still require provider reads before execution.
pub(super) enum DesiredPlanFile {
    Verified(RehydratedPlanFile),
    NeedsHistoricalVerification(DesiredVerifiedPlanFile),
}

enum SavedPlanExecutionConfig {
    WithoutCurrentConfig(Box<ValidatedConfig<()>>),
    WithCurrentConfig(Box<ValidatedConfig<ExpandedCanonicalResources<InferredExportParam>>>),
}

impl SavedPlanExecutionConfig {
    fn context(&self) -> &WiringContext {
        match self {
            Self::WithoutCurrentConfig(config) => config.context(),
            Self::WithCurrentConfig(config) => config.context(),
        }
    }
}

/// A desired saved plan bound to the exact validated provider context used
/// for artifact verification and, when needed, current-config validation.
pub(super) struct ValidatedDesiredPlan {
    plan: DesiredPlanFile,
    config: SavedPlanExecutionConfig,
}

impl ValidatedDesiredPlan {
    fn without_current_config(plan: RehydratedPlanFile, config: ValidatedConfig<()>) -> Self {
        Self {
            plan: DesiredPlanFile::Verified(plan),
            config: SavedPlanExecutionConfig::WithoutCurrentConfig(Box::new(config)),
        }
    }

    fn with_current_config(
        plan: DesiredPlanFile,
        config: ValidatedConfig<ExpandedCanonicalResources<InferredExportParam>>,
    ) -> Self {
        Self {
            plan,
            config: SavedPlanExecutionConfig::WithCurrentConfig(Box::new(config)),
        }
    }

    pub(super) fn context(&self) -> &WiringContext {
        self.config.context()
    }

    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        self.plan.provider_configs()
    }

    pub(super) fn sorted_resources(&self) -> &[SavedResource] {
        self.plan.sorted_resources()
    }

    pub(super) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        self.plan.current_states()
    }

    pub(super) fn historical_resource_addresses(&self) -> HistoricalResourceAddresses {
        self.plan.historical_resource_addresses()
    }

    pub(super) async fn verify_historical(
        self,
        provider: &dyn Provider,
        actual_states: &mut HashMap<ResourceId, State>,
        verified_planned_states: &mut HashMap<ResourceId, State>,
        historical_addresses: &HistoricalResourceAddresses,
        apply_target: ApplyTarget<'_>,
    ) -> Result<ValidatedExecutablePlan, AppError> {
        let Self { plan, config } = self;
        let plan = plan
            .verify_historical(
                provider,
                actual_states,
                verified_planned_states,
                historical_addresses,
                apply_target,
            )
            .await?;
        Ok(ValidatedExecutablePlan { plan, config })
    }
}

/// Executable saved-plan payload retaining its validated configuration owner.
pub(super) struct ValidatedExecutablePlan {
    plan: ExecutablePlanFile,
    config: SavedPlanExecutionConfig,
}

impl ValidatedExecutablePlan {
    pub(super) fn context(&self) -> &WiringContext {
        self.config.context()
    }
}

impl std::ops::Deref for ValidatedExecutablePlan {
    type Target = ExecutablePlanFile;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

impl DesiredPlanFile {
    pub(super) fn provider_configs(&self) -> &SavedProviderConfigs {
        match self {
            Self::Verified(plan) => plan.provider_configs(),
            Self::NeedsHistoricalVerification(plan) => plan.provider_configs(),
        }
    }

    pub(super) fn sorted_resources(&self) -> &[SavedResource] {
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
        let mut plan = match self {
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
        plan.plan_file
            .plan_mut()
            .rebase_ignored_update_sources(actual_states);
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

    pub(super) fn sorted_resources(&self) -> &[SavedResource] {
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
pub(crate) struct RehydratedPlanFile {
    plan_file: ReloadedPlanFile,
    planned_secret_hashes: PlannedSecretHashes,
}

impl RehydratedPlanFile {
    fn plan(&self) -> &SavedPlan {
        self.plan_file.plan()
    }

    fn provider_configs(&self) -> &SavedProviderConfigs {
        self.plan_file.provider_configs()
    }

    fn sorted_resources(&self) -> &[SavedResource] {
        self.plan_file.sorted_resources()
    }

    fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        &self.planned_secret_hashes
    }

    fn unresolved_resources(&self) -> &[SavedResource] {
        self.plan_file.unresolved_resources()
    }

    fn compositions(&self) -> &[SavedComposition] {
        self.plan_file.compositions()
    }

    fn data_sources(&self) -> &[SavedDataSource] {
        self.plan_file.data_sources()
    }

    fn data_source_origins(&self) -> &[SavedDataSource] {
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
    pub(super) fn reloaded_plan_file(&self) -> &ReloadedPlanFile {
        &self.plan.plan_file
    }

    pub(super) fn plan(&self) -> &SavedPlan {
        self.plan.plan()
    }

    pub(super) fn sorted_resources(&self) -> &[SavedResource] {
        self.plan.sorted_resources()
    }

    pub(super) fn planned_secret_hashes(&self) -> &PlannedSecretHashes {
        self.plan.planned_secret_hashes()
    }

    pub(super) fn unresolved_resources(&self) -> &[SavedResource] {
        self.plan.unresolved_resources()
    }

    pub(super) fn compositions(&self) -> &[SavedComposition] {
        self.plan.compositions()
    }

    pub(super) fn data_sources(&self) -> &[SavedDataSource] {
        self.plan.data_sources()
    }

    pub(super) fn data_source_origins(&self) -> &[SavedDataSource] {
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
    use carina_core::effect::Effect;
    use carina_core::parser::ProviderConfig;
    use carina_core::plan::Plan;
    use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
    use carina_core::resource::{
        ConcreteValue, Current, DataSource, DeferredValue, ResolvedResourceId, Resource,
        ResourceId, State, Value,
    };
    use carina_state::{ResourceState, StateFile};
    use indexmap::IndexMap;

    use super::{
        DesiredVerifiedPlanFile, HistoricalResourceAddresses, InspectedPlanFile,
        ReloadedVerifiedPlanFile, SchemaCheckedPendingPlanFile, VerifiedPlanFile,
    };
    use crate::commands::apply::ApplyTarget;
    use crate::commands::plan::{
        CurrentStateEntry, PlanFile, PlanFileContents, RecordedProviderArtifact,
        SavedDataSourceValue, SavedManagedCurrent,
    };
    use crate::error::AppError;
    use crate::wiring::{
        LoadedProviderFactory, ProviderLoad, SavedPlanReplayMoves, ValidatedConfig,
    };

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

    fn wasm_context(name: &'static str, bytes: &[u8]) -> ValidatedConfig<()> {
        ValidatedConfig::for_test(
            (),
            vec![LoadedProviderFactory::for_test_with_bytes(
                Box::new(ArtifactFactory { name }),
                bytes,
            )],
            &[provider_config(name)],
        )
    }

    fn wasm_load(name: &'static str, bytes: &[u8]) -> ProviderLoad {
        ProviderLoad::for_test(
            vec![LoadedProviderFactory::for_test_with_bytes(
                Box::new(ArtifactFactory { name }),
                bytes,
            )],
            &[provider_config(name)],
        )
    }

    fn builtin_context() -> ValidatedConfig<()> {
        ValidatedConfig::from_factories_for_test((), Vec::new(), &[])
    }

    fn builtin_load() -> ProviderLoad {
        ProviderLoad::for_test(Vec::new(), &[])
    }

    fn saved_provider_configs(
        configs: Vec<ProviderConfig>,
    ) -> carina_core::parser::SavedProviderConfigs {
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers = configs;
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

    fn plan_recorded_with<R>(
        ctx: &ValidatedConfig<R>,
        provider_configs: Vec<ProviderConfig>,
    ) -> PlanFile {
        empty_plan_contents(provider_configs)
            .record_for_test(ctx)
            .expect("test plan provider evidence must be valid")
    }

    fn apply_target() -> ApplyTarget<'static> {
        ApplyTarget::SavedPlan {
            plan_file: Path::new("reviewed-plan.json"),
            source_dir: Path::new("."),
        }
    }

    fn reloaded(plan: PlanFile) -> ReloadedVerifiedPlanFile {
        let InspectedPlanFile::Verified(VerifiedPlanFile {
            plan_file,
            schema_references,
        }) = InspectedPlanFile::inspect(plan)
        else {
            panic!("artifact fixture unexpectedly contains secret placeholders")
        };
        ReloadedVerifiedPlanFile {
            plan_file: plan_file
                .reload_in_flight_from_locked_state(None)
                .expect("reload saved plan"),
            schema_references,
        }
    }

    fn schema_checked_move_plan(
        from: ResourceId,
        to: ResourceId,
        current: Current,
    ) -> (SchemaCheckedPendingPlanFile, Option<StateFile>) {
        let mut contents = empty_plan_contents(Vec::new());
        contents.plan.add(Effect::Move {
            from: ResolvedResourceId::new(from.clone()),
            to: ResolvedResourceId::new(to.clone()),
            origin: None,
        });
        contents.current_states.push(CurrentStateEntry::Managed {
            id: to,
            current: SavedManagedCurrent::from_current(current),
        });
        let mut source_row = ResourceState::try_applied(
            from.resource_type.clone(),
            from.identity().cloned().expect("move source is resolved"),
            from.provider.clone(),
            "resource-id",
        )
        .expect("fixture identifier is non-empty");
        source_row.directives.provider_instance = from.provider_instance.clone();
        let mut locked_state = StateFile::new();
        locked_state
            .upsert_resource(source_row)
            .expect("test state setup must be valid");
        let plan_file = contents
            .record_for_test(&builtin_context())
            .expect("test plan provider evidence must be valid")
            .reload_in_flight_from_locked_state(Some(&locked_state))
            .expect("reload saved move plan")
            .resolve_with_schemas(&carina_core::schema::SchemaRegistry::new())
            .expect("resolve saved move plan");
        (
            SchemaCheckedPendingPlanFile { plan_file },
            Some(locked_state),
        )
    }

    #[test]
    fn saved_move_replay_keeps_routed_desired_target_placeholder() {
        let from = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "old",
            Some("source-route".to_string()),
        );
        let to = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "new",
            Some("target-route".to_string()),
        );
        let planned = Current::from_state(
            State::existing(to.clone(), HashMap::new()).with_identifier("resource-id"),
        );
        let (pending, state_file) = schema_checked_move_plan(from.clone(), to.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[Resource::from_id(to.clone())],
                &SavedPlanReplayMoves::for_test_explicit([from.clone()]),
                &state_file,
            )
            .expect("saved move replay state must restore");

        assert!(prior.orphans.contains_key(&from));
        assert!(!prior.managed.contains_key(&from));
        assert!(matches!(
            prior.managed.get(&to),
            Some(Current::Absent(state)) if state.state().id == to
        ));
    }

    #[test]
    fn saved_move_replay_keeps_absent_source_as_an_orphan() {
        let from = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "old",
            Some("source-route".to_string()),
        );
        let to = ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "new",
            Some("target-route".to_string()),
        );
        let planned = Current::absent(to.clone());
        let (pending, state_file) = schema_checked_move_plan(from.clone(), to.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[Resource::from_id(to.clone())],
                &SavedPlanReplayMoves::for_test_explicit([from.clone()]),
                &state_file,
            )
            .expect("saved absent move replay state must restore");

        assert!(matches!(
            prior.orphans.get(&from).map(|orphan| orphan.current()),
            Some(Current::Absent(state)) if state.state().id == from
        ));
        assert!(!prior.managed.contains_key(&from));
        assert!(matches!(
            prior.managed.get(&to),
            Some(Current::Absent(state)) if state.state().id == to
        ));
    }

    #[test]
    fn saved_move_replay_retains_moved_orphan_refresh_exclusion() {
        let from = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let to = ResourceId::with_provider_identity("mock", "test.resource", "retired", None);
        let planned = Current::from_state(
            State::existing(to.clone(), HashMap::new()).with_identifier("resource-id"),
        );
        let (pending, state_file) = schema_checked_move_plan(from.clone(), to.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[],
                &SavedPlanReplayMoves::for_test_explicit([from.clone()]),
                &state_file,
            )
            .expect("saved moved orphan replay state must restore");

        assert!(prior.orphans.contains_key(&from));
        assert!(!prior.managed.contains_key(&from));
        assert!(
            prior.already_refreshed.contains(&to),
            "the post-move orphan address was refreshed when the plan was recorded"
        );
    }

    #[test]
    fn saved_early_automatic_move_restores_its_origin_for_replay() {
        let from = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let to = ResourceId::with_provider_identity("mock", "test.resource", "fresh", None);
        let planned = Current::from_state(
            State::existing(to.clone(), HashMap::new()).with_identifier("resource-id"),
        );
        let (pending, state_file) = schema_checked_move_plan(from.clone(), to.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[Resource::from_id(to.clone())],
                &SavedPlanReplayMoves::for_test_automatic([from.clone()]),
                &state_file,
            )
            .expect("early automatic move replay state must restore");

        assert!(prior.managed.contains_key(&from));
        assert!(matches!(prior.managed.get(&to), Some(Current::Absent(_))));
    }

    #[test]
    fn saved_late_automatic_move_remains_at_target_when_late_detection_is_not_replayed() {
        let from = ResourceId::with_provider_identity("mock", "test.resource", "old", None);
        let to = ResourceId::with_provider_identity("mock", "test.resource", "fresh", None);
        let planned = Current::from_state(
            State::existing(to.clone(), HashMap::new()).with_identifier("resource-id"),
        );
        let (pending, state_file) = schema_checked_move_plan(from.clone(), to.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[Resource::from_id(to.clone())],
                &SavedPlanReplayMoves::for_test_automatic([]),
                &state_file,
            )
            .expect("late automatic move target must remain addressed");

        assert!(!prior.managed.contains_key(&from));
        assert!(matches!(prior.managed.get(&to), Some(Current::Present(_))));
    }

    #[test]
    fn saved_chained_move_restores_root_and_creates_intermediate_placeholder() {
        let root = ResourceId::with_provider_identity("mock", "test.resource", "root", None);
        let intermediate =
            ResourceId::with_provider_identity("mock", "test.resource", "intermediate", None);
        let target = ResourceId::with_provider_identity("mock", "test.resource", "target", None);
        let planned = Current::from_state(
            State::existing(target.clone(), HashMap::new()).with_identifier("resource-id"),
        );
        let (pending, state_file) = schema_checked_move_plan(root.clone(), target.clone(), planned);

        let prior = pending
            .expansion_prior_states(
                &[Resource::from_id(intermediate.clone())],
                &SavedPlanReplayMoves::for_test_automatic([root.clone()]),
                &state_file,
            )
            .expect("chained move replay state must restore");

        assert!(prior.managed.contains_key(&root));
        assert!(matches!(
            prior.managed.get(&intermediate),
            Some(Current::Absent(_))
        ));
        assert!(!prior.managed.contains_key(&target));
        assert!(prior.already_refreshed.contains(&target));
    }

    #[test]
    fn changed_artifact_with_identical_schemas_fails_before_schema_checked_state() {
        let planned = wasm_context("mock", b"artifact A");
        let loaded = wasm_load("mock", b"artifact B");

        let result = reloaded(plan_recorded_with(&planned, vec![provider_config("mock")]))
            .verify_provider_load(
                loaded
                    .into_complete()
                    .expect("test provider load is complete"),
                apply_target(),
            );

        let Err(AppError::SavedPlanProviderArtifactMismatch(mismatch)) = result else {
            panic!("changed provider bytes must not produce a checked plan")
        };
        assert_eq!(mismatch.providers.keys().collect::<Vec<_>>(), vec!["mock"]);
    }

    #[test]
    fn record_rejects_artifact_origin_that_does_not_match_config_source() {
        let context = wasm_context("mock", b"artifact");
        let error = empty_plan_contents(vec![remote_provider_config("mock", "carina-rs/mock")])
            .record_for_test(&context)
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
        let loaded = wasm_load("mock", b"artifact A");

        let result = reloaded(plan_recorded_with(&planned, vec![provider_config("mock")]))
            .verify_provider_load(
                loaded
                    .into_complete()
                    .expect("test provider load is complete"),
                apply_target(),
            );

        assert!(result.is_ok());
    }

    #[test]
    fn saved_plan_verification_bundles_checked_plan_with_checked_wiring() {
        let planned = wasm_context("mock", b"artifact A");
        let loaded = wasm_load("mock", b"artifact A")
            .into_complete()
            .expect("test provider load is complete");

        let checked = reloaded(plan_recorded_with(&planned, vec![provider_config("mock")]))
            .verify_provider_load(loaded, apply_target())
            .expect("matching artifacts and schemas must verify together");

        assert!(checked.context().provider_artifacts().contains_key("mock"));
    }

    #[test]
    fn builtin_artifact_records_and_verifies_as_builtin() {
        let planned = builtin_context();
        let plan = plan_recorded_with(&planned, Vec::new());
        assert_eq!(
            plan.provider_artifacts().get("mock"),
            Some(&RecordedProviderArtifact::BuiltIn)
        );

        let loaded = builtin_load()
            .into_complete()
            .expect("test provider load is complete");
        let result = reloaded(plan).verify_provider_load(loaded, apply_target());

        assert!(result.is_ok());
    }

    #[test]
    fn builtin_vs_wasm_and_missing_or_extra_provider_keys_are_mismatches() {
        let builtin_plan = plan_recorded_with(&builtin_context(), Vec::new());

        let same_name = reloaded(builtin_plan.clone()).verify_provider_load(
            wasm_load("mock", b"artifact")
                .into_complete()
                .expect("test provider load is complete"),
            apply_target(),
        );
        assert!(matches!(
            same_name,
            Err(AppError::SavedPlanProviderArtifactMismatch(ref mismatch))
                if mismatch.providers.contains_key("mock")
        ));

        let changed_keys = reloaded(builtin_plan).verify_provider_load(
            wasm_load("other", b"artifact")
                .into_complete()
                .expect("test provider load is complete"),
            apply_target(),
        );
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
        .record_for_test(&builtin_context())
        .expect("test plan provider evidence must be valid")
        .reload_in_flight_from_locked_state(None)
        .expect("reload saved plan")
        .resolve_with_schemas(&carina_core::schema::SchemaRegistry::new())
        .expect("resolve saved plan");
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
